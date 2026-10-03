use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;
use thiserror::Error;

use crate::{
    api::{ApiError, ControllerClient},
    config::{Cli, ControllerSettings, config_home, state_home},
    routing::{self, AutoPolicy},
    ui::human_bytes,
};

/// Providers pick the output format from the user agent; this one yields Clash Meta YAML.
const USER_AGENT: &str = "clash-verge/2.0.0";
const MAX_SUBSCRIPTION_BYTES: usize = 8 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Minimum gap between automatic attempts after a failure.
const RETRY_DELAY: Duration = Duration::from_secs(600);
const USERINFO_HEADER: &str = "subscription-userinfo";

#[derive(Debug, Clone)]
pub struct SubscriptionSettings {
    pub link_file: PathBuf,
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub interval: Duration,
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error("cannot locate {0}; set HOME or pass it explicitly")]
    NoDefaultPath(&'static str),
    #[error("failed to read subscription link {path}: {source}")]
    ReadLink {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("subscription link file {0} is empty")]
    EmptyLink(PathBuf),
    #[error("download failed: {0}")]
    Fetch(String),
    #[error("provider returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("downloaded subscription is not a usable Clash config: {0}")]
    Invalid(String),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("core rejected the new config ({0}); previous config restored")]
    Rejected(String),
    #[error("config saved, but reloading the core failed: {0}")]
    Reload(ApiError),
    #[error("another update is already running")]
    Busy,
    #[error("routing policy: {0}")]
    Routing(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// New config written and applied by the running core.
    Reloaded,
    /// Provider served the same config (ignoring comments); nothing was written.
    Unchanged,
    /// Last success is younger than the interval.
    NotDue { next_in: Duration },
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reloaded => formatter.write_str("Subscription updated and reloaded"),
            Self::Unchanged => formatter.write_str("Subscription unchanged"),
            Self::NotDue { next_in } => write!(
                formatter,
                "Subscription is fresh; next update in {}",
                human_duration(*next_in)
            ),
        }
    }
}

/// Persisted between runs so the timer, the CLI and the dashboard share one schedule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionState {
    /// Unix seconds of the last attempt, successful or not.
    pub last_attempt: Option<u64>,
    /// Unix seconds of the last attempt that ended with a valid config in place.
    pub last_success: Option<u64>,
    pub last_error: Option<String>,
    pub usage: Option<Usage>,
}

/// Durable undo record for the config, shared provider, source and policy as one update.
#[derive(Serialize, Deserialize)]
struct PendingUpdate {
    config_path: PathBuf,
    files: Vec<(PathBuf, Option<String>)>,
    selected: Vec<(String, String)>,
    reload: bool,
}

/// Use the pre-update controller credentials even if the interrupted config is invalid.
pub(crate) fn recovery_config(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let state = config_state_dir(path)?;
    if state_home().is_some_and(|home| home.join("clash-man/pending.json").exists()) {
        return Err(std::io::Error::other(
            "legacy update pending; run recovery with the previous binary before migration",
        ));
    }
    let bytes = match fs::read(state.join("pending.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let pending: PendingUpdate = serde_json::from_slice(&bytes)
        .map_err(|_| std::io::Error::other("invalid update recovery journal"))?;
    if fs::canonicalize(path)? != fs::canonicalize(&pending.config_path)? {
        return Err(std::io::Error::other(
            "pending update belongs to another config; recover using its --config path",
        ));
    }
    Ok(pending.files.into_iter().find_map(|(file, old)| {
        (file == pending.config_path)
            .then_some(old)
            .flatten()
            .map(String::into_bytes)
    }))
}

fn save_pending(
    settings: &SubscriptionSettings,
    pending: &PendingUpdate,
) -> Result<(), SubscriptionError> {
    write_atomically(
        &settings.state_dir.join("pending.json"),
        &serde_json::to_vec(pending).expect("recovery record serializes"),
    )
}

fn clear_pending(settings: &SubscriptionSettings) -> Result<(), SubscriptionError> {
    let path = settings.state_dir.join("pending.json");
    fs::remove_file(&path).map_err(|source| SubscriptionError::Write { path, source })?;
    sync_dir(&settings.state_dir)
}

async fn rollback_update(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    pending: &PendingUpdate,
) -> Result<(), SubscriptionError> {
    for (path, old) in pending.files.iter().rev() {
        if let Some(bytes) = old {
            write_atomically(path, bytes.as_bytes())?;
        } else if path.exists() {
            fs::remove_file(path).map_err(|source| SubscriptionError::Write {
                path: path.clone(),
                source,
            })?;
            if let Some(parent) = path.parent() {
                sync_dir(parent)?;
            }
        }
    }
    if pending.reload && pending.config_path.exists() {
        client
            .reload_config(&pending.config_path)
            .await
            .map_err(SubscriptionError::Reload)?;
        restore_selections(client, &pending.selected).await?;
    }
    clear_pending(settings)
}

/// All manager writes to a core share the configuration's transaction lock.
pub fn configuration_lock(settings: &SubscriptionSettings) -> Result<fs::File, SubscriptionError> {
    prepare_state(&settings.state_dir)?;
    let lock_path = settings.state_dir.join("update.lock");
    let lock = fs::File::create(&lock_path).map_err(|source| SubscriptionError::Write {
        path: lock_path,
        source,
    })?;
    if lock.try_lock().is_err() {
        return Err(SubscriptionError::Busy);
    }
    Ok(lock)
}

pub async fn recover(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
) -> Result<(), SubscriptionError> {
    let _lock = configuration_lock(settings)?;
    recover_pending(settings, client).await
}

async fn recover_pending(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
) -> Result<(), SubscriptionError> {
    let path = settings.state_dir.join("pending.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SubscriptionError::Write { path, source }),
    };
    let pending: PendingUpdate = serde_json::from_slice(&bytes)
        .map_err(|_| SubscriptionError::Routing("invalid update recovery journal".into()))?;
    if fs::canonicalize(&settings.config_path).ok() != fs::canonicalize(&pending.config_path).ok()
        || settings.config_path != pending.config_path && !settings.config_path.exists()
    {
        return Err(SubscriptionError::Routing(
            "pending update belongs to another config; recover using its --config path".into(),
        ));
    }
    rollback_update(settings, client, &pending).await
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    /// Unix seconds; absent when the plan never expires.
    pub expire: Option<u64>,
}

impl Usage {
    /// e.g. `81.4 GiB of 167.8 GiB (48%) · expires in 241d`.
    pub fn summary(&self, now: u64) -> String {
        let used = self.upload.saturating_add(self.download);
        let mut text = if self.total > 0 {
            format!(
                "{} of {} ({}%)",
                human_bytes(used),
                human_bytes(self.total),
                used.saturating_mul(100) / self.total
            )
        } else {
            format!("{} used", human_bytes(used))
        };
        if let Some(expire) = self.expire {
            if expire > now {
                text += &format!(
                    " · expires in {}",
                    human_duration(Duration::from_secs(expire - now))
                );
            } else {
                text += " · expired";
            }
        }
        text
    }
}

impl SubscriptionSettings {
    pub fn resolve(cli: &Cli, controller: &ControllerSettings) -> Result<Self, SubscriptionError> {
        let config_path = match &controller.config_path {
            Some(path) => path.clone(),
            None => config_home()
                .ok_or(SubscriptionError::NoDefaultPath("the Clash config"))?
                .join("clash/config.yaml"),
        };
        let link_file = match &cli.sublink_file {
            Some(path) => path.clone(),
            None => config_path.with_file_name("sublink.txt"),
        };
        let state_dir =
            config_state_dir(&config_path).map_err(|source| SubscriptionError::Write {
                path: config_path.clone(),
                source,
            })?;
        Ok(Self {
            link_file,
            config_path,
            state_dir,
            interval: cli.update_interval,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.state_dir.join("subscription.json")
    }

    fn backup_path(&self) -> PathBuf {
        self.state_dir.join("config.yaml.bak")
    }

    pub fn routing_path(&self) -> PathBuf {
        self.state_dir.join("routing.json")
    }

    fn source_path(&self) -> PathBuf {
        self.state_dir.join("source.yaml")
    }

    fn provider_path(&self) -> PathBuf {
        self.state_dir.join("nodes.yaml")
    }

    fn canonical_config(&self) -> PathBuf {
        fs::canonicalize(&self.config_path).unwrap_or_else(|_| self.config_path.clone())
    }

    pub fn routing_policy(&self) -> Result<Option<AutoPolicy>, SubscriptionError> {
        let path = if self.routing_path().exists() {
            self.routing_path()
        } else {
            self.canonical_config().with_extension("routing.json")
        };
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| SubscriptionError::Routing("invalid routing policy file".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SubscriptionError::Write {
                path: self.routing_path(),
                source,
            }),
        }
    }

    pub fn load_state(&self) -> SubscriptionState {
        fs::read(self.state_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save_state(&self, state: &SubscriptionState) -> Result<(), SubscriptionError> {
        let bytes = serde_json::to_vec_pretty(state).expect("state serializes");
        write_atomically(&self.state_path(), &bytes)
    }
}

impl SubscriptionState {
    /// Time until an automatic update should run; zero means due now.
    pub fn next_in(&self, interval: Duration, now: u64) -> Duration {
        let after = |time: Option<u64>, gap: Duration| {
            time.map_or(0, |time| time.saturating_add(gap.as_secs()))
        };
        let due_at = after(self.last_success, interval).max(after(self.last_attempt, RETRY_DELAY));
        Duration::from_secs(due_at.saturating_sub(now))
    }
}

/// Fetches the subscription and swaps it in, unless it is not due and `force` is unset.
pub async fn update(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    force: bool,
) -> Result<Outcome, SubscriptionError> {
    let _lock = configuration_lock(settings)?;

    recover_pending(settings, client).await?;

    let mut state = settings.load_state();
    let now = unix_now();
    let next_in = state.next_in(settings.interval, now);
    if !force && !next_in.is_zero() {
        return Ok(Outcome::NotDue { next_in });
    }

    state.last_attempt = Some(now);
    let result = fetch_and_apply(settings, client, &mut state).await;
    match &result {
        Ok(_) => {
            state.last_success = Some(now);
            state.last_error = None;
        }
        Err(error) => state.last_error = Some(error.to_string()),
    }
    let saved = settings.save_state(&state);
    let outcome = result?;
    saved.map(|()| outcome)
}

async fn fetch_and_apply(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    state: &mut SubscriptionState,
) -> Result<Outcome, SubscriptionError> {
    let link =
        fs::read_to_string(&settings.link_file).map_err(|source| SubscriptionError::ReadLink {
            path: settings.link_file.clone(),
            source,
        })?;
    let link = link.trim();
    if link.is_empty() {
        return Err(SubscriptionError::EmptyLink(settings.link_file.clone()));
    }

    let body = download(link, state).await?;
    validate(&body)?;

    let policy = settings.routing_policy()?;
    apply_configuration(settings, client, &body, policy.as_ref()).await
}

/// Change policy without fetching a subscription or depending on an open dashboard.
pub async fn configure_auto(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    mut policy: AutoPolicy,
) -> Result<Outcome, SubscriptionError> {
    policy.validate().map_err(SubscriptionError::Routing)?;
    let _lock = configuration_lock(settings)?;
    recover_pending(settings, client).await?;
    // Policy changes need a live core: do not report an unverified mode change as applied.
    client.version().await.map_err(SubscriptionError::Reload)?;
    let previous = settings.routing_policy()?;
    if !policy.enabled {
        if let Some(mut old) = previous.clone() {
            old.enabled = false;
            policy = old;
        } else {
            return Ok(Outcome::Unchanged);
        }
    }
    let source_path = if previous.is_some() {
        if settings.source_path().exists() {
            settings.source_path()
        } else {
            settings
                .canonical_config()
                .with_extension("subscription.yaml")
        }
    } else {
        settings.config_path.clone()
    };
    let source = fs::read_to_string(&source_path).map_err(|source| SubscriptionError::Write {
        path: source_path,
        source,
    })?;
    validate(&source)?;
    apply_configuration(settings, client, &source, Some(&policy)).await
}

async fn apply_configuration(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    source: &str,
    policy: Option<&AutoPolicy>,
) -> Result<Outcome, SubscriptionError> {
    let config_path =
        fs::canonicalize(&settings.config_path).unwrap_or_else(|_| settings.config_path.clone());
    let current = match fs::read_to_string(&config_path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(SubscriptionError::Write {
                path: config_path.clone(),
                source,
            });
        }
    };
    let source = routing::preserve_host_settings(source, current.as_deref())
        .map_err(SubscriptionError::Routing)?;
    let provider_path = settings.provider_path();
    let rendered =
        routing::render(&source, policy, &provider_path).map_err(SubscriptionError::Routing)?;
    let mut changes = Vec::new();
    if let Some(provider) = rendered.provider {
        changes.push((settings.provider_path(), provider.into_bytes()));
    }
    if let Some(policy) = policy {
        changes.push((settings.source_path(), source.into_bytes()));
        changes.push((
            settings.routing_path(),
            serde_json::to_vec_pretty(policy).expect("routing policy serializes"),
        ));
    }
    changes.push((config_path.clone(), rendered.config.into_bytes()));
    let mut snapshots = Vec::new();
    for (path, bytes) in changes {
        let old = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(SubscriptionError::Write { path, source }),
        };
        if old.as_ref().is_some_and(|old| {
            old == &bytes
                || match (std::str::from_utf8(old), std::str::from_utf8(&bytes)) {
                    (Ok(old), Ok(new)) => same_ignoring_comments(old, new),
                    _ => false,
                }
        }) {
            continue;
        }
        snapshots.push((path, old, bytes));
    }
    if snapshots.is_empty() {
        return Ok(Outcome::Unchanged);
    }
    let selected = client
        .proxies()
        .await
        .ok()
        .map(|payload| {
            payload
                .proxies
                .into_iter()
                .filter(|(_, proxy)| proxy.kind == "Selector" && !proxy.now.is_empty())
                .map(|(group, proxy)| (group, proxy.now))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(current) = &current {
        write_atomically(&settings.backup_path(), current.as_bytes())?;
    }
    let mut pending = PendingUpdate {
        config_path: config_path.clone(),
        files: snapshots
            .iter()
            .map(|(path, old, _)| {
                let text = old
                    .as_ref()
                    .map(|bytes| String::from_utf8(bytes.clone()))
                    .transpose()
                    .map_err(|_| {
                        SubscriptionError::Routing("existing configuration is not UTF-8".into())
                    })?;
                Ok((path.clone(), text))
            })
            .collect::<Result<_, SubscriptionError>>()?,
        selected: selected.clone(),
        reload: false,
    };
    save_pending(settings, &pending)?;

    let applied = async {
        for (path, _, bytes) in &snapshots {
            write_atomically(path, bytes)?;
        }
        pending.reload = true;
        save_pending(settings, &pending)?;
        match client.reload_config(&config_path).await {
            Ok(()) => {
                restore_selections(client, &selected).await?;
                if let Some(policy) = policy.filter(|policy| policy.enabled) {
                    verify_automatic(client, policy).await?;
                }
                Ok(Outcome::Reloaded)
            }
            Err(ApiError::Rejected(message)) => {
                pending.reload = false; // The core explicitly rejected before applying it.
                Err(SubscriptionError::Rejected(message))
            }
            Err(error) => Err(SubscriptionError::Reload(error)),
        }
    }
    .await;
    if applied.is_err() {
        rollback_update(settings, client, &pending).await?;
    } else {
        clear_pending(settings)?;
    }
    applied
}

async fn verify_automatic(
    client: &ControllerClient,
    policy: &AutoPolicy,
) -> Result<(), SubscriptionError> {
    let automatic = policy.automatic_group();
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let payload = client.proxies().await.map_err(SubscriptionError::Reload)?;
        if !payload
            .proxies
            .get(&policy.group)
            .is_some_and(|group| group.now == automatic && group.all == [automatic.clone()])
            || !payload
                .proxies
                .get(&automatic)
                .is_some_and(|group| group.kind == "Fallback" && !group.all.is_empty())
        {
            return Err(SubscriptionError::Routing(
                "core did not activate the automatic group".into(),
            ));
        }
        let selected = &payload.proxies[&automatic].now;
        if payload
            .proxies
            .get(selected)
            .is_some_and(|node| node.alive == Some(true) && !node.history.is_empty())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(SubscriptionError::Routing(
                "no automatically selected node passed the native health check within 40s".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn restore_selections(
    client: &ControllerClient,
    selected: &[(String, String)],
) -> Result<(), SubscriptionError> {
    if selected.is_empty() {
        return Ok(());
    }
    let current = client.proxies().await.map_err(SubscriptionError::Reload)?;
    for (group, node) in selected {
        if current
            .proxies
            .get(group)
            .is_some_and(|proxy| proxy.kind == "Selector" && proxy.all.contains(node))
        {
            client
                .select_proxy(group, node)
                .await
                .map_err(SubscriptionError::Reload)?;
        }
    }
    Ok(())
}

async fn download(link: &str, state: &mut SubscriptionState) -> Result<String, SubscriptionError> {
    let fetch_error =
        |error: reqwest::Error| SubscriptionError::Fetch(error.without_url().to_string());
    // Direct connection: the proxy may be the thing that is broken.
    let http = reqwest::Client::builder()
        .no_proxy()
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(fetch_error)?;
    let response = http.get(link).send().await.map_err(fetch_error)?;
    if !response.status().is_success() {
        return Err(SubscriptionError::HttpStatus(response.status()));
    }
    let usage = parse_usage(response.headers());
    let bytes = crate::api::bounded_body(response, MAX_SUBSCRIPTION_BYTES)
        .await
        .map_err(|error| SubscriptionError::Fetch(error.to_string()))?;
    let body = String::from_utf8(bytes)
        .map_err(|_| SubscriptionError::Invalid("response is not UTF-8 text".into()))?;
    if usage.is_some() {
        state.usage = usage;
    }
    Ok(body)
}

/// Rejects error pages, base64 node lists and configs without any proxies.
fn validate(body: &str) -> Result<(), SubscriptionError> {
    let value: Value = serde_yaml_ng::from_str(body)
        .map_err(|error| SubscriptionError::Invalid(format!("not YAML: {error}")))?;
    let Some(root) = value.as_mapping() else {
        return Err(SubscriptionError::Invalid("not a YAML mapping".into()));
    };
    let proxies = root
        .get("proxies")
        .and_then(Value::as_sequence)
        .map_or(0, Vec::len);
    let providers = root
        .get("proxy-providers")
        .and_then(Value::as_mapping)
        .map_or(0, serde_yaml_ng::Mapping::len);
    if proxies + providers == 0 {
        return Err(SubscriptionError::Invalid(
            "no proxies or proxy-providers".into(),
        ));
    }
    Ok(())
}

/// Providers stamp every download with a timestamp comment; ignore comment lines.
fn same_ignoring_comments(left: &str, right: &str) -> bool {
    fn content(text: &str) -> impl Iterator<Item = &str> {
        text.lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .map(str::trim_end)
    }
    content(left).eq(content(right))
}

/// Parses `subscription-userinfo: upload=1; download=2; total=3; expire=4`.
fn parse_usage(headers: &HeaderMap) -> Option<Usage> {
    let raw = headers.get(USERINFO_HEADER)?.to_str().ok()?;
    let mut usage = Usage::default();
    for pair in raw.split(';') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        // Some providers send floats such as `1.2e10`.
        let Ok(number) = value.trim().parse::<f64>() else {
            continue;
        };
        let number = number.max(0.0) as u64;
        match key.trim() {
            "upload" => usage.upload = number,
            "download" => usage.download = number,
            "total" => usage.total = number,
            "expire" if number > 0 => usage.expire = Some(number),
            _ => {}
        }
    }
    Some(usage)
}

fn config_state_dir(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::canonicalize(path.parent().unwrap_or(Path::new(".")))?.join(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("missing config filename"))?,
            )
        }
        Err(error) => return Err(error),
    };
    let mut name = canonical.as_os_str().to_owned();
    name.push(".clash-man");
    Ok(PathBuf::from(name))
}

fn prepare_state(path: &Path) -> Result<(), SubscriptionError> {
    create_dir(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        SubscriptionError::Write {
            path: path.to_owned(),
            source,
        }
    })?;
    if !path.join(".gitignore").exists() {
        write_atomically(&path.join(".gitignore"), b"*\n")?;
    }
    Ok(())
}

fn create_dir(path: &Path) -> Result<(), SubscriptionError> {
    fs::create_dir_all(path).map_err(|source| SubscriptionError::Write {
        path: path.to_owned(),
        source,
    })
}

/// Writes through a sibling temp file so a crash never leaves a truncated file behind.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SubscriptionError> {
    let write_error = |source| SubscriptionError::Write {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(
        ".clash-man.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let temp = PathBuf::from(temp);
    let write = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&temp);
        return Err(write_error(error));
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        write_error(error)
    })?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Flushes a directory entry after a create/rename/remove. Directory handles are
/// not openable on Windows, where NTFS already orders the metadata write.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<(), SubscriptionError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| SubscriptionError::Write {
            path: path.to_owned(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<(), SubscriptionError> {
    Ok(())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};

    use axum::{
        Json, Router,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, put},
    };
    use serde_json::{Value as JsonValue, json};
    use tokio::{net::TcpListener, sync::Mutex};

    use super::*;
    use crate::config::ControllerSettings;

    const CONFIG_V1: &str = "# updated 1\nproxies:\n  - {name: a, type: ss}\nproxy-groups: []\n";
    const CONFIG_V1_RESTAMPED: &str =
        "# updated 2\nproxies:\n  - {name: a, type: ss}\nproxy-groups: []\n";
    const CONFIG_V2: &str = "proxies:\n  - {name: b, type: ss}\n";

    #[derive(Clone)]
    struct Mock {
        body: Arc<Mutex<&'static str>>,
        reload_status: Arc<Mutex<StatusCode>>,
        reloads: Arc<Mutex<Vec<JsonValue>>>,
        proxies: Arc<Mutex<JsonValue>>,
        selection_failures: Arc<Mutex<u32>>,
    }

    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}/")
    }

    struct Fixture {
        _dir: TempDir,
        settings: SubscriptionSettings,
        client: ControllerClient,
        mock: Mock,
    }

    async fn fixture(initial_config: Option<&str>) -> Fixture {
        let mock = Mock {
            body: Arc::new(Mutex::new(CONFIG_V1)),
            reload_status: Arc::new(Mutex::new(StatusCode::NO_CONTENT)),
            reloads: Arc::default(),
            proxies: Arc::new(Mutex::new(json!({"proxies": {}}))),
            selection_failures: Arc::default(),
        };
        let router = Router::new()
            .route(
                "/version",
                get(|| async { Json(json!({"meta":true,"version":"test"})) }),
            )
            .route(
                "/proxies",
                get(
                    |axum::extract::State(mock): axum::extract::State<Mock>| async move {
                        Json(mock.proxies.lock().await.clone())
                    },
                ),
            )
            .route(
                "/proxies/{group}",
                put(
                    |axum::extract::State(mock): axum::extract::State<Mock>| async move {
                        let mut failures = mock.selection_failures.lock().await;
                        if *failures > 0 {
                            *failures -= 1;
                            StatusCode::INTERNAL_SERVER_ERROR
                        } else {
                            StatusCode::NO_CONTENT
                        }
                    },
                ),
            )
            .route(
                "/sub",
                get(
                    |axum::extract::State(mock): axum::extract::State<Mock>| async move {
                        (
                            [(
                                USERINFO_HEADER,
                                "upload=10; download=20; total=100; expire=0",
                            )],
                            *mock.body.lock().await,
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/configs",
                put(
                    |axum::extract::State(mock): axum::extract::State<Mock>,
                     Json(payload): Json<JsonValue>| async move {
                        mock.reloads.lock().await.push(payload);
                        let status = *mock.reload_status.lock().await;
                        (status, Json(json!({ "message": "bad config" }))).into_response()
                    },
                ),
            )
            .with_state(mock.clone());
        let base = serve(router).await;

        let dir = TempDir::new();
        let config_path = dir.path().join("config.yaml");
        if let Some(config) = initial_config {
            fs::write(&config_path, config).unwrap();
        }
        let link_file = dir.path().join("sublink.txt");
        fs::write(&link_file, format!("{base}sub\n")).unwrap();
        let settings = SubscriptionSettings {
            link_file,
            config_path,
            state_dir: dir.path().join("state"),
            interval: Duration::from_secs(3_600),
        };
        let client = ControllerClient::new(&ControllerSettings {
            base_url: url::Url::parse(&base).unwrap(),
            secret: String::new(),
            config_path: None,
            group_test_urls: BTreeMap::new(),
        })
        .unwrap();
        Fixture {
            _dir: dir,
            settings,
            client,
            mock,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "clash-man-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn replaces_config_reloads_core_and_records_usage() {
        let fixture = fixture(Some(CONFIG_V2)).await;
        let outcome = update(&fixture.settings, &fixture.client, false)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Reloaded);
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            CONFIG_V1
        );
        assert_eq!(
            fs::read_to_string(fixture.settings.backup_path()).unwrap(),
            CONFIG_V2
        );
        let reloads = fixture.mock.reloads.lock().await;
        assert_eq!(reloads.len(), 1);
        assert!(
            reloads[0]["path"]
                .as_str()
                .unwrap()
                .ends_with("config.yaml")
        );

        let state = fixture.settings.load_state();
        assert!(state.last_success.is_some());
        assert_eq!(state.last_error, None);
        assert_eq!(
            state.usage,
            Some(Usage {
                upload: 10,
                download: 20,
                total: 100,
                expire: None
            })
        );
    }

    #[tokio::test]
    async fn respects_interval_unless_forced() {
        let fixture = fixture(None).await;
        update(&fixture.settings, &fixture.client, false)
            .await
            .unwrap();
        assert!(matches!(
            update(&fixture.settings, &fixture.client, false).await,
            Ok(Outcome::NotDue { .. })
        ));
        *fixture.mock.body.lock().await = CONFIG_V1_RESTAMPED;
        assert_eq!(
            update(&fixture.settings, &fixture.client, true)
                .await
                .unwrap(),
            Outcome::Unchanged
        );
        assert_eq!(fixture.mock.reloads.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn keeps_old_config_when_download_is_not_clash_yaml() {
        let fixture = fixture(Some(CONFIG_V2)).await;
        *fixture.mock.body.lock().await = "c3M6Ly9ub2RlLWxpc3Q=";
        let error = update(&fixture.settings, &fixture.client, false)
            .await
            .unwrap_err();
        assert!(matches!(error, SubscriptionError::Invalid(_)));
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            CONFIG_V2
        );
        let state = fixture.settings.load_state();
        assert!(state.last_error.is_some());
        assert!(state.last_attempt.is_some());
        assert_eq!(state.last_success, None);
        assert!(
            !state
                .next_in(Duration::from_secs(3_600), unix_now())
                .is_zero()
        );
    }

    #[tokio::test]
    async fn restores_previous_config_when_core_rejects_it() {
        let fixture = fixture(Some(CONFIG_V2)).await;
        *fixture.mock.reload_status.lock().await = StatusCode::BAD_REQUEST;
        let error = update(&fixture.settings, &fixture.client, false)
            .await
            .unwrap_err();
        assert!(
            matches!(error, SubscriptionError::Rejected(ref message) if message == "bad config")
        );
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            CONFIG_V2
        );
    }

    const ROUTABLE: &str = "proxies: [{name: a, type: ss, password: old}]\nproxy-groups: [{name: Proxy, type: select, proxies: [a]}]\nrules: ['MATCH,Proxy']\n";

    fn auto_policy() -> AutoPolicy {
        AutoPolicy {
            enabled: true,
            group: "Proxy".into(),
            preferred: vec!["a".into()],
            interval_seconds: 30,
            test_url: routing::DEFAULT_TEST_URL.into(),
            direct_fallback: false,
            http_fallback: None,
        }
    }

    async fn mock_automatic(fixture: &Fixture) {
        *fixture.mock.proxies.lock().await = json!({"proxies": {
            "Proxy": {"type":"Selector","now":"Proxy Auto","all":["Proxy Auto"]},
            "Proxy Auto": {"type":"Fallback","now":"a","all":["a"]},
            "a": {"type":"Shadowsocks","alive":true,"history":[{"delay":50}]}
        }});
    }

    #[tokio::test]
    async fn automatic_policy_survives_subscription_and_can_be_disabled() {
        let fixture = fixture(Some(ROUTABLE)).await;
        mock_automatic(&fixture).await;
        let mut policy = auto_policy();
        policy.direct_fallback = true;
        policy.http_fallback = Some("127.0.0.1:17890".parse().unwrap());
        configure_auto(&fixture.settings, &fixture.client, policy.clone())
            .await
            .unwrap();
        assert_eq!(
            fixture.settings.routing_policy().unwrap(),
            Some(policy.clone())
        );
        assert!(fixture.settings.provider_path().is_file());
        *fixture.mock.body.lock().await = "proxies: [{name: a, type: ss, password: new}]\nproxy-groups: [{name: Proxy, type: select, proxies: [a]}]\nrules: ['MATCH,Proxy']\n";
        assert_eq!(
            update(&fixture.settings, &fixture.client, true)
                .await
                .unwrap(),
            Outcome::Reloaded
        );
        let nodes = fs::read_to_string(fixture.settings.provider_path()).unwrap();
        assert!(nodes.contains("password: new"));
        assert!(nodes.contains("clash-man-direct-fallback"));
        assert!(nodes.contains("clash-man-http-fallback"));
        assert_eq!(fixture.settings.routing_policy().unwrap(), Some(policy));
        assert!(
            fs::read_to_string(&fixture.settings.config_path)
                .unwrap()
                .contains("type: fallback")
        );
        assert_eq!(
            update(&fixture.settings, &fixture.client, true)
                .await
                .unwrap(),
            Outcome::Unchanged
        );
        let mut manual = auto_policy();
        manual.enabled = false;
        configure_auto(&fixture.settings, &fixture.client, manual)
            .await
            .unwrap();
        let config = fs::read_to_string(&fixture.settings.config_path).unwrap();
        assert!(!config.contains("type: fallback"));
        assert!(config.contains("password: new"));
        assert!(!fixture.settings.routing_policy().unwrap().unwrap().enabled);
    }

    #[tokio::test]
    async fn failed_automatic_activation_restores_every_file() {
        let fixture = fixture(Some(ROUTABLE)).await;
        // The mock accepts reload but never exposes the requested automatic group.
        let error = configure_auto(&fixture.settings, &fixture.client, auto_policy())
            .await
            .unwrap_err();
        assert!(matches!(error, SubscriptionError::Routing(_)));
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            ROUTABLE
        );
        assert!(!fixture.settings.provider_path().exists());
        assert!(!fixture.settings.source_path().exists());
        assert!(!fixture.settings.routing_path().exists());
        assert_eq!(fixture.mock.reloads.lock().await.len(), 2);
        assert!(!fixture.settings.state_dir.join("pending.json").exists());
    }

    #[tokio::test]
    async fn ordinary_update_restores_running_core_after_selection_failure() {
        let fixture = fixture(Some(CONFIG_V2)).await;
        *fixture.mock.proxies.lock().await = json!({"proxies": {
            "Other": {"type":"Selector", "now":"DIRECT", "all":["DIRECT"]}
        }});
        *fixture.mock.selection_failures.lock().await = 1;
        assert!(
            update(&fixture.settings, &fixture.client, true)
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            CONFIG_V2
        );
        assert_eq!(fixture.mock.reloads.lock().await.len(), 2);
        assert!(!fixture.settings.state_dir.join("pending.json").exists());
    }

    #[tokio::test]
    async fn next_invocation_recovers_an_interrupted_multi_file_update() {
        let fixture = fixture(Some(ROUTABLE)).await;
        create_dir(&fixture.settings.state_dir).unwrap();
        let pending = PendingUpdate {
            config_path: fixture.settings.config_path.clone(),
            files: vec![
                (
                    fixture.settings.config_path.clone(),
                    Some(ROUTABLE.to_owned()),
                ),
                (fixture.settings.provider_path(), None),
                (fixture.settings.routing_path(), None),
            ],
            selected: Vec::new(),
            reload: true,
        };
        save_pending(&fixture.settings, &pending).unwrap();
        fs::write(&fixture.settings.config_path, "partial generation").unwrap();
        fs::write(fixture.settings.provider_path(), "new nodes").unwrap();
        fs::write(fixture.settings.routing_path(), "partial policy").unwrap();
        fixture
            .settings
            .save_state(&SubscriptionState {
                last_success: Some(unix_now()),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(
            update(&fixture.settings, &fixture.client, false)
                .await
                .unwrap(),
            Outcome::NotDue { .. }
        ));
        assert_eq!(
            fs::read_to_string(&fixture.settings.config_path).unwrap(),
            ROUTABLE
        );
        assert!(!fixture.settings.provider_path().exists());
        assert!(!fixture.settings.routing_path().exists());
        assert!(!fixture.settings.state_dir.join("pending.json").exists());
        assert_eq!(fixture.mock.reloads.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn bad_subscription_keeps_active_automatic_policy_and_nodes() {
        let fixture = fixture(Some(ROUTABLE)).await;
        mock_automatic(&fixture).await;
        configure_auto(&fixture.settings, &fixture.client, auto_policy())
            .await
            .unwrap();
        let original = fs::read(&fixture.settings.config_path).unwrap();
        let nodes = fs::read(fixture.settings.provider_path()).unwrap();
        *fixture.mock.body.lock().await = CONFIG_V2;
        assert!(
            update(&fixture.settings, &fixture.client, true)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&fixture.settings.config_path).unwrap(), original);
        assert_eq!(fs::read(fixture.settings.provider_path()).unwrap(), nodes);
        assert_eq!(
            fixture.settings.routing_policy().unwrap(),
            Some(auto_policy())
        );
    }

    #[test]
    fn schedules_by_success_and_backs_off_after_failure() {
        let interval = Duration::from_secs(3_600);
        assert!(
            SubscriptionState::default()
                .next_in(interval, 1_000)
                .is_zero()
        );
        let fresh = SubscriptionState {
            last_attempt: Some(1_000),
            last_success: Some(1_000),
            ..Default::default()
        };
        assert_eq!(fresh.next_in(interval, 1_600), Duration::from_secs(3_000));
        let failed = SubscriptionState {
            last_attempt: Some(10_000),
            last_success: Some(1_000),
            ..Default::default()
        };
        assert_eq!(failed.next_in(interval, 10_100), Duration::from_secs(500));
    }

    #[test]
    fn parses_float_usage_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            USERINFO_HEADER,
            "upload=1.5e3; download=2; total=3; expire=1811491078"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_usage(&headers),
            Some(Usage {
                upload: 1_500,
                download: 2,
                total: 3,
                expire: Some(1_811_491_078)
            })
        );
    }
}
