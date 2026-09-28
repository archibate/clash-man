use std::{
    fmt, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;
use thiserror::Error;

use crate::{
    api::{ApiError, ControllerClient},
    config::{Cli, ControllerSettings, config_home, state_home},
    ui::human_bytes,
};

/// Providers pick the output format from the user agent; this one yields Clash Meta YAML.
const USER_AGENT: &str = "clash-verge/2.0.0";
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// New config written and applied by the running core.
    Reloaded,
    /// New config written; the core is not running and will read it on start.
    SavedOffline,
    /// Provider served the same config (ignoring comments); nothing was written.
    Unchanged,
    /// Last success is younger than the interval.
    NotDue { next_in: Duration },
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reloaded => formatter.write_str("Subscription updated and reloaded"),
            Self::SavedOffline => {
                formatter.write_str("Subscription updated; core offline, applies on next start")
            }
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
        let state_dir = state_home()
            .ok_or(SubscriptionError::NoDefaultPath("the state directory"))?
            .join("clash-man");
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
    create_dir(&settings.state_dir)?;
    let lock_path = settings.state_dir.join("update.lock");
    let lock = fs::File::create(&lock_path).map_err(|source| SubscriptionError::Write {
        path: lock_path,
        source,
    })?;
    if lock.try_lock().is_err() {
        return Err(SubscriptionError::Busy);
    }

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

    let config_path =
        fs::canonicalize(&settings.config_path).unwrap_or_else(|_| settings.config_path.clone());
    let current = fs::read_to_string(&config_path).ok();
    if current
        .as_deref()
        .is_some_and(|current| same_ignoring_comments(current, &body))
    {
        return Ok(Outcome::Unchanged);
    }

    if let Some(current) = &current {
        write_atomically(&settings.backup_path(), current.as_bytes())?;
    }
    write_atomically(&config_path, body.as_bytes())?;

    match client.reload_config(&config_path).await {
        Ok(()) => Ok(Outcome::Reloaded),
        Err(ApiError::Transport(_)) => Ok(Outcome::SavedOffline),
        Err(ApiError::Rejected(message)) => {
            if let Some(current) = &current {
                write_atomically(&config_path, current.as_bytes())?;
            }
            Err(SubscriptionError::Rejected(message))
        }
        Err(error) => Err(SubscriptionError::Reload(error)),
    }
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
    let bytes = response.bytes().await.map_err(fetch_error)?;
    let body = String::from_utf8(bytes.to_vec())
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
    temp.push(".clash-man.tmp");
    let temp = PathBuf::from(temp);
    fs::write(&temp, bytes).map_err(write_error)?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        write_error(error)
    })
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
        };
        let router = Router::new()
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
