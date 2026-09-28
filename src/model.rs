use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VersionInfo {
    #[serde(default)]
    pub meta: bool,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProxyPayload {
    #[serde(default)]
    pub proxies: BTreeMap<String, ProxyInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProxyInfo {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub now: String,
    #[serde(default)]
    pub all: Vec<String>,
    #[serde(default)]
    pub history: Vec<DelayHistory>,
    pub alive: Option<bool>,
    pub udp: Option<bool>,
    pub xudp: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DelayHistory {
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub delay: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsPayload {
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub download_total: u64,
    #[serde(default)]
    pub upload_total: u64,
    #[serde(default)]
    pub memory: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub metadata: ConnectionMetadata,
    #[serde(default)]
    pub upload: u64,
    #[serde(default)]
    pub download: u64,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub chains: Vec<String>,
    #[serde(default)]
    pub rule: String,
    #[serde(default)]
    pub rule_payload: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionMetadata {
    #[serde(default)]
    pub network: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "sourceIP", default)]
    pub source_ip: String,
    #[serde(default)]
    pub source_port: String,
    #[serde(rename = "destinationIP", default)]
    pub destination_ip: String,
    #[serde(default)]
    pub destination_port: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub process: String,
    #[serde(default)]
    pub process_path: String,
    #[serde(default)]
    pub dns_mode: String,
}

impl Connection {
    pub fn destination(&self) -> String {
        let host = if self.metadata.host.is_empty() {
            &self.metadata.destination_ip
        } else {
            &self.metadata.host
        };
        format!("{}:{}", host, self.metadata.destination_port)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RulesPayload {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rule {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub proxy: String,
    #[serde(default)]
    pub size: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RuntimeConfig {
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub socks_port: u16,
    #[serde(default)]
    pub mixed_port: u16,
    #[serde(default)]
    pub redir_port: u16,
    #[serde(default)]
    pub tproxy_port: u16,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub allow_lan: bool,
    #[serde(default)]
    pub ipv6: bool,
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub tun: TunConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TunConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub stack: String,
    #[serde(rename = "auto-route", default)]
    pub auto_route: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrafficSample {
    #[serde(default)]
    pub up: u64,
    #[serde(default)]
    pub down: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemorySample {
    #[serde(default)]
    pub inuse: u64,
    #[serde(default)]
    pub oslimit: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogEntry {
    #[serde(rename = "type", alias = "level", default)]
    pub level: String,
    #[serde(default, alias = "message")]
    pub payload: String,
    #[serde(default)]
    pub time: String,
}
