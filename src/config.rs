use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

const DEFAULT_CONTROLLER: &str = "http://127.0.0.1:9090";

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    /// Clash/Mihomo external-controller URL.
    #[arg(long, env = "CLASH_MAN_CONTROLLER", global = true)]
    pub controller: Option<String>,

    /// Clash/Mihomo YAML configuration used for controller discovery and subscription updates.
    #[arg(long, env = "CLASH_MAN_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Read the controller secret from this file.
    #[arg(long, global = true)]
    pub secret_file: Option<PathBuf>,

    /// File holding the subscription URL [default: sublink.txt beside the config].
    #[arg(long, env = "CLASH_MAN_SUBLINK_FILE", global = true)]
    pub sublink_file: Option<PathBuf>,

    /// Minimum age before the subscription is fetched again (e.g. 30m, 6h, 1d).
    #[arg(
        long,
        env = "CLASH_MAN_UPDATE_INTERVAL",
        default_value = "6h",
        value_parser = parse_interval,
        global = true
    )]
    pub update_interval: Duration,

    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Roll back an interrupted update without fetching a subscription.
    Recover,
    /// Configure native automatic failover (continues when this dashboard is closed).
    Auto {
        /// Routing group to manage.
        #[arg(long, default_value = "Proxy")]
        group: String,
        /// Preferred node, in priority order; repeat this option for backups.
        #[arg(long = "prefer")]
        preferred: Vec<String>,
        /// Active health-check interval (5s to 1d).
        #[arg(long, default_value = "30s", value_parser = parse_interval)]
        interval: Duration,
        /// URL used by the core to test node health.
        #[arg(long, default_value = crate::routing::DEFAULT_TEST_URL)]
        test_url: String,
        /// Restore the subscription's manual routing groups.
        #[arg(long)]
        disable: bool,
    },
    /// Download the subscription, validate it, replace the config and hot-reload the core.
    Update {
        /// Update even if the last success is younger than --update-interval.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone)]
pub struct ControllerSettings {
    pub base_url: Url,
    pub secret: String,
    pub config_path: Option<PathBuf>,
    pub group_test_urls: BTreeMap<String, String>,
}

impl std::fmt::Debug for ControllerSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerSettings")
            .field("base_url", &self.base_url)
            .field("secret", &"<redacted>")
            .field("config_path", &self.config_path)
            .field("group_test_urls", &self.group_test_urls)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("invalid controller URL: {0}")]
    InvalidController(#[from] url::ParseError),
}

#[derive(Debug, Default, Deserialize)]
struct ClashConfig {
    #[serde(rename = "external-controller")]
    external_controller: Option<String>,
    secret: Option<String>,
    #[serde(rename = "proxy-groups", default)]
    proxy_groups: Vec<ProxyGroupConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ProxyGroupConfig {
    #[serde(default)]
    name: String,
    url: Option<String>,
}

impl ControllerSettings {
    pub fn resolve(cli: &Cli) -> Result<Self, ConfigError> {
        let config_path = discover_config(cli.config.as_deref())
            .map(|path| fs::canonicalize(&path).unwrap_or(path));
        let file_config = config_path
            .as_deref()
            .filter(|path| path.is_file())
            .map(read_clash_config)
            .transpose()?
            .unwrap_or_default();

        let controller = cli
            .controller
            .clone()
            .or_else(|| env::var("CLASH_MAN_CONTROLLER").ok())
            .or(file_config.external_controller)
            .unwrap_or_else(|| DEFAULT_CONTROLLER.to_owned());

        let secret = match &cli.secret_file {
            Some(path) => fs::read_to_string(path)
                .map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?
                .trim()
                .to_owned(),
            None => env::var("CLASH_MAN_SECRET")
                .ok()
                .or(file_config.secret)
                .unwrap_or_default(),
        };
        let group_test_urls = file_config
            .proxy_groups
            .into_iter()
            .filter_map(|group| group.url.map(|url| (group.name, url)))
            .collect();

        Ok(Self {
            base_url: normalize_controller(&controller)?,
            secret,
            config_path,
            group_test_urls,
        })
    }
}

fn discover_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_owned());
    }

    let config_home = config_home()?;
    ["mihomo/config.yaml", "clash/config.yaml"]
        .into_iter()
        .map(|suffix| config_home.join(suffix))
        .find(|path| path.is_file())
}

pub fn config_home() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".config")))
}

pub fn state_home() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/state")))
}

fn parse_interval(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let split = raw
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(raw.len());
    let (amount, unit) = raw.split_at(split);
    let amount: u64 = amount
        .parse()
        .map_err(|_| format!("expected a number with unit s/m/h/d, got {raw:?}"))?;
    let seconds = match unit {
        "s" => 1,
        "" | "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    let seconds = amount.checked_mul(seconds).ok_or("interval is too large")?;
    Ok(Duration::from_secs(seconds))
}

fn read_clash_config(path: &Path) -> Result<ClashConfig, ConfigError> {
    let bytes = crate::subscription::recovery_config(path)
        .and_then(|old| old.map_or_else(|| fs::read(path), Ok))
        .map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
    serde_yaml_ng::from_slice(&bytes).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source,
    })
}

fn normalize_controller(raw: &str) -> Result<Url, url::ParseError> {
    let with_scheme = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let mut url = Url::parse(&with_scheme)?;
    if matches!(url.host_str(), Some("0.0.0.0" | "::")) {
        url.set_host(Some("127.0.0.1"))?;
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{normalize_controller, parse_interval};

    #[test]
    fn parses_update_intervals() {
        assert_eq!(parse_interval("6h"), Ok(Duration::from_secs(21_600)));
        assert_eq!(parse_interval("90"), Ok(Duration::from_secs(5_400)));
        assert_eq!(parse_interval("1d"), Ok(Duration::from_secs(86_400)));
        assert!(parse_interval("6x").is_err());
        assert!(parse_interval("h").is_err());
    }

    #[test]
    fn normalizes_bare_and_wildcard_controllers() {
        assert_eq!(
            normalize_controller("127.0.0.1:9090").unwrap().as_str(),
            "http://127.0.0.1:9090/"
        );
        assert_eq!(
            normalize_controller("0.0.0.0:9090").unwrap().as_str(),
            "http://127.0.0.1:9090/"
        );
    }
}
