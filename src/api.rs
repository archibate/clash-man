use std::{path::Path, sync::Arc, time::Duration};

use reqwest::{Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::mpsc;
use url::Url;

use crate::{
    config::ControllerSettings,
    model::{
        ConnectionsPayload, LogEntry, MemorySample, ProxyPayload, RulesPayload, RuntimeConfig,
        TrafficSample, VersionInfo,
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-node budget the core gets for a latency test.
const DELAY_TEST_MS: u64 = 5_000;
/// The core tests a group's nodes concurrently, so one budget plus slack covers the call.
const GROUP_DELAY_TIMEOUT: Duration = Duration::from_millis(DELAY_TEST_MS + 5_000);
const MAX_STREAM_LINE: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("controller rejected authentication")]
    Unauthorized,
    #[error("controller returned HTTP {0}")]
    Status(StatusCode),
    #[error("controller rejected the request: {0}")]
    Rejected(String),
    #[error("controller transport failed: {0}")]
    Transport(String),
    #[error("invalid controller response: {0}")]
    Decode(String),
    #[error("controller stream line exceeded 1 MiB")]
    StreamTooLarge,
    #[error("event receiver closed")]
    ReceiverClosed,
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Clone)]
pub struct ControllerClient {
    base_url: Url,
    secret: Arc<str>,
    http: reqwest::Client,
}

impl ControllerClient {
    pub fn new(settings: &ControllerSettings) -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            base_url: settings.base_url.clone(),
            secret: Arc::from(settings.secret.as_str()),
            http,
        })
    }

    pub async fn version(&self) -> ApiResult<VersionInfo> {
        self.get("version").await
    }

    pub async fn proxies(&self) -> ApiResult<ProxyPayload> {
        self.get("proxies").await
    }

    pub async fn connections(&self) -> ApiResult<ConnectionsPayload> {
        self.get("connections").await
    }

    pub async fn rules(&self) -> ApiResult<RulesPayload> {
        self.get("rules").await
    }

    pub async fn configs(&self) -> ApiResult<RuntimeConfig> {
        self.get("configs").await
    }

    pub async fn select_proxy(&self, group: &str, proxy: &str) -> ApiResult<()> {
        let url = self.named_endpoint("proxies", group)?;
        self.empty_request(
            Method::PUT,
            url,
            Some(&serde_json::json!({ "name": proxy })),
        )
        .await
    }

    pub async fn group_delay(&self, group: &str, test_url: &str) -> ApiResult<serde_json::Value> {
        let mut url = self.named_endpoint("group", group)?;
        url.path_segments_mut()
            .map_err(|_| ApiError::Decode("controller URL cannot hold path segments".into()))?
            .push("delay");
        url.query_pairs_mut()
            .append_pair("url", test_url)
            .append_pair("timeout", &DELAY_TEST_MS.to_string());
        self.get_url(url, GROUP_DELAY_TIMEOUT).await
    }

    pub async fn close_connection(&self, id: &str) -> ApiResult<()> {
        let url = self.named_endpoint("connections", id)?;
        self.empty_request::<serde_json::Value>(Method::DELETE, url, None)
            .await
    }

    pub async fn close_all_connections(&self) -> ApiResult<()> {
        let url = self.endpoint("connections")?;
        self.empty_request::<serde_json::Value>(Method::DELETE, url, None)
            .await
    }

    pub async fn patch_config(&self, patch: serde_json::Value) -> ApiResult<()> {
        let url = self.endpoint("configs")?;
        self.empty_request(Method::PATCH, url, Some(&patch)).await
    }

    /// Makes the core re-read its configuration file, rebuilding listeners like a restart would.
    pub async fn reload_config(&self, path: &Path) -> ApiResult<()> {
        let mut url = self.endpoint("configs")?;
        url.query_pairs_mut().append_pair("force", "true");
        let response = self
            .authorized(
                self.http
                    .put(url)
                    .timeout(RELOAD_TIMEOUT)
                    .json(&serde_json::json!({ "path": path })),
            )
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() == StatusCode::BAD_REQUEST {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::Rejected(controller_message(&body)));
        }
        validate_status(response)?;
        Ok(())
    }

    pub async fn stream_traffic(&self, sender: mpsc::Sender<TrafficSample>) -> ApiResult<()> {
        self.stream_json("traffic", sender).await
    }

    pub async fn stream_memory(&self, sender: mpsc::Sender<MemorySample>) -> ApiResult<()> {
        self.stream_json("memory", sender).await
    }

    pub async fn stream_logs(&self, sender: mpsc::Sender<LogEntry>) -> ApiResult<()> {
        self.stream_json("logs?level=debug", sender).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        let url = self.endpoint(path)?;
        self.get_url(url, REQUEST_TIMEOUT).await
    }

    async fn get_url<T: DeserializeOwned>(&self, url: Url, timeout: Duration) -> ApiResult<T> {
        let response = self
            .authorized(self.http.get(url).timeout(timeout))
            .send()
            .await
            .map_err(transport_error)?;
        let response = validate_status(response)?;
        response.json().await.map_err(decode_error)
    }

    async fn empty_request<T: Serialize + ?Sized>(
        &self,
        method: Method,
        url: Url,
        body: Option<&T>,
    ) -> ApiResult<()> {
        let mut request = self.authorized(self.http.request(method, url).timeout(REQUEST_TIMEOUT));
        if let Some(body) = body {
            request = request.json(body);
        }
        validate_status(request.send().await.map_err(transport_error)?)?;
        Ok(())
    }

    async fn stream_json<T>(&self, path: &str, sender: mpsc::Sender<T>) -> ApiResult<()>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let url = self.endpoint(path)?;
        let response = self
            .authorized(self.http.get(url))
            .send()
            .await
            .map_err(transport_error)?;
        let mut response = validate_status(response)?;
        let mut buffer = Vec::new();

        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            buffer.extend_from_slice(&chunk);
            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=position).collect::<Vec<_>>();
                let text = std::str::from_utf8(&line)
                    .map_err(|error| ApiError::Decode(error.to_string()))?
                    .trim();
                if text.is_empty() {
                    continue;
                }
                let value = serde_json::from_str(text).map_err(decode_error)?;
                sender
                    .send(value)
                    .await
                    .map_err(|_| ApiError::ReceiverClosed)?;
            }
            if buffer.len() > MAX_STREAM_LINE {
                return Err(ApiError::StreamTooLarge);
            }
        }
        Err(ApiError::Transport("controller stream ended".into()))
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.secret.is_empty() {
            request
        } else {
            request.bearer_auth(&*self.secret)
        }
    }

    fn endpoint(&self, path: &str) -> ApiResult<Url> {
        self.base_url
            .join(path)
            .map_err(|error| ApiError::Decode(error.to_string()))
    }

    fn named_endpoint(&self, resource: &str, name: &str) -> ApiResult<Url> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| ApiError::Decode("controller URL cannot hold path segments".into()))?
            .extend([resource, name]);
        Ok(url)
    }
}

fn validate_status(response: reqwest::Response) -> ApiResult<reqwest::Response> {
    match response.status() {
        StatusCode::UNAUTHORIZED => Err(ApiError::Unauthorized),
        status if status.is_client_error() || status.is_server_error() => {
            Err(ApiError::Status(status))
        }
        _ => Ok(response),
    }
}

fn transport_error(error: reqwest::Error) -> ApiError {
    ApiError::Transport(error.without_url().to_string())
}

fn controller_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned())
}

fn decode_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use axum::{
        Json, Router,
        body::Body,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        response::Response,
        routing::{get, put},
    };
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Mutex};

    use super::{ApiError, ControllerClient};
    use crate::config::ControllerSettings;

    #[derive(Clone, Default)]
    struct MockState {
        selected: Arc<Mutex<Option<(String, String, String)>>>,
    }

    async fn mock_server(router: Router) -> url::Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        url::Url::parse(&format!("http://{address}/")).unwrap()
    }

    fn client(base_url: url::Url, secret: &str) -> ControllerClient {
        ControllerClient::new(&ControllerSettings {
            base_url,
            secret: secret.into(),
            config_path: None,
            group_test_urls: BTreeMap::new(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn authenticates_and_percent_encodes_proxy_names() {
        let state = MockState::default();
        let router = Router::new()
            .route(
                "/proxies/{name}",
                put(
                    |State(state): State<MockState>,
                     Path(name): Path<String>,
                     headers: HeaderMap,
                     Json(payload): Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        *state.selected.lock().await = Some((
                            name,
                            payload["name"].as_str().unwrap_or_default().to_owned(),
                            auth,
                        ));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(state.clone());
        let client = client(mock_server(router).await, "secret value");

        client
            .select_proxy("手动 选择", "日本 中继 01")
            .await
            .unwrap();

        assert_eq!(
            state.selected.lock().await.as_ref().unwrap(),
            &(
                "手动 选择".into(),
                "日本 中继 01".into(),
                "Bearer secret value".into()
            )
        );
    }

    #[tokio::test]
    async fn maps_unauthorized_without_leaking_the_url() {
        let router = Router::new().route("/version", get(|| async { StatusCode::UNAUTHORIZED }));
        let error = client(mock_server(router).await, "wrong")
            .version()
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Unauthorized));
        assert_eq!(error.to_string(), "controller rejected authentication");
    }

    #[tokio::test]
    async fn parses_chunked_newline_json_streams() {
        let router = Router::new().route(
            "/traffic",
            get(|| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("{\"up\":1,\"down\":2}\n{\"up\":3,\"down\":4}\n"))
                    .unwrap()
            }),
        );
        let client = client(mock_server(router).await, "");
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(async move { client.stream_traffic(sender).await });

        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        assert_eq!((first.up, first.down), (1, 2));
        assert_eq!((second.up, second.down), (3, 4));
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn deserializes_legacy_meta_payloads() {
        let router = Router::new().route(
            "/proxies",
            get(|| async {
                Json(json!({
                    "proxies": {
                        "Proxy": {"type": "Selector", "now": "香港 01", "all": ["香港 01"]},
                        "香港 01": {"type": "Shadowsocks", "udp": true, "history": []}
                    }
                }))
            }),
        );
        let payload = client(mock_server(router).await, "")
            .proxies()
            .await
            .unwrap();
        assert_eq!(payload.proxies["Proxy"].now, "香港 01");
        assert_eq!(payload.proxies["香港 01"].udp, Some(true));
    }

    #[tokio::test]
    async fn preserves_legacy_uppercase_ip_fields() {
        let router = Router::new().route(
            "/connections",
            get(|| async {
                Json(json!({
                    "connections": [{
                        "id": "one",
                        "metadata": {
                            "sourceIP": "127.0.0.1",
                            "sourcePort": "1234",
                            "destinationIP": "1.1.1.1",
                            "destinationPort": "443"
                        }
                    }]
                }))
            }),
        );
        let payload = client(mock_server(router).await, "")
            .connections()
            .await
            .unwrap();
        assert_eq!(payload.connections[0].metadata.source_ip, "127.0.0.1");
        assert_eq!(payload.connections[0].metadata.destination_ip, "1.1.1.1");
    }
}
