//! Lightweight HTTP reverse proxy for AI Context Fence.
//!
//! Injects fence authentication (master key or OIDC JWT) into requests while
//! preserving the upstream provider's Authorization header. This allows CLI tools
//! like Codex to use their subscription credentials transparently through the fence.

use actix_web::{
    http::{header::HeaderMap, StatusCode as ActixStatus},
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use actix_ws::AggregatedMessage;
use anyhow::{Context, Result};
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use std::future::{pending, Future};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

const LOCAL_PROXY_MAX_WEBSOCKET_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const LOCAL_PROXY_WEBSOCKET_CONNECT_ATTEMPTS: usize = 3;
const LOCAL_PROXY_WEBSOCKET_CONNECT_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const LOCAL_PROXY_UPSTREAM_IDLE_PING_INTERVAL: Duration = Duration::from_secs(25);
#[cfg(test)]
const LOCAL_PROXY_UPSTREAM_IDLE_PING_INTERVAL: Duration = Duration::from_millis(50);

type LocalProxyUpstreamWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Configuration for the local proxy.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Fence server URL (e.g., "https://fence.example.com").
    pub fence_url: String,
    /// Authentication method for the fence server.
    pub auth_method: AuthMethod,
    /// Local port to listen on.
    pub listen_port: u16,
    /// Trusted correlation headers injected into every forwarded request.
    pub correlation_headers: Vec<(String, String)>,
    /// Optional locally-generated API key that the proxy validates itself.
    /// When set, incoming requests must present this key in the Authorization
    /// header or Anthropic-style x-api-key header.
    pub local_api_key: Option<String>,
    /// When true, accept OpenAI-style access tokens (JWTs) in addition to local_api_key.
    pub subscription_mode: bool,
    /// Optional env var containing a provider bearer token that the proxy injects
    /// when the client request does not already include Authorization.
    pub provider_auth_env_var: Option<String>,
    /// When set, inject X-Fence-Protocol-Diffs-Dir header so the fence backend
    /// writes protocol diff files (incoming vs transformed request/response) into
    /// this directory.
    pub protocol_diffs_dir: Option<PathBuf>,
    /// Print local proxy startup details to stderr.
    pub verbose: bool,
    /// Runtime hook used by the proprietary binary to retain proxy metrics.
    pub observe_request_duration: fn(f64),
}

/// How the proxy authenticates to the fence server.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// Master key sent via X-Fence-Auth header.
    MasterKey(String),
    /// OIDC JWT sent via X-Fence-Auth header.
    OidcToken(String),
    /// Session-scoped gateway key loaded from a file for each proxied request.
    GatewayKeyFile(PathBuf),
    /// Session-scoped gateway key supplied directly.
    GatewayKey(String),
    /// Per-user API key sent via X-Fence-Auth header.
    ApiKey(String),
}

impl AuthMethod {
    /// Load auth method: prefer OIDC token if available, fall back to master key.
    pub fn resolve(
        master_key: Option<String>,
        use_oidc: bool,
        gateway_key: Option<String>,
        gateway_key_file: Option<PathBuf>,
        api_key: Option<String>,
    ) -> Result<Self> {
        if let Some(path) = gateway_key_file {
            Ok(AuthMethod::GatewayKeyFile(path))
        } else if let Some(key) = gateway_key {
            let key = key.trim().to_string();
            if key.is_empty() {
                anyhow::bail!("gateway key must not be empty");
            }
            Ok(AuthMethod::GatewayKey(key))
        } else if let Some(key) = api_key {
            let key = key.trim().to_string();
            if key.is_empty() {
                anyhow::bail!("API key must not be empty");
            }
            Ok(AuthMethod::ApiKey(key))
        } else if use_oidc {
            let creds = crate::auth::load_credentials()?
                .context("No stored OIDC credentials. Run `ai-fence-cli auth login` first.")?;
            if creds.is_expired() {
                anyhow::bail!(
                    "Stored OIDC token is expired. Run `ai-fence-cli auth login` to refresh."
                );
            }
            Ok(AuthMethod::OidcToken(creds.access_token))
        } else if let Some(key) = master_key {
            Ok(AuthMethod::MasterKey(key))
        } else if let Some(creds) = crate::auth::load_credentials()? {
            if creds.is_expired() {
                anyhow::bail!("Stored OIDC token is expired. Run `ai-fence-cli login` to refresh.");
            }
            Ok(AuthMethod::OidcToken(creds.access_token))
        } else {
            anyhow::bail!(
                "No proxy authentication configured. Run `ai-fence-cli login`, or pass --master-key for admin-only use."
            );
        }
    }

    /// Build the fence auth headers to inject.
    pub fn headers(&self) -> Result<Vec<(&'static str, String)>> {
        match self {
            AuthMethod::MasterKey(key) => Ok(vec![("x-fence-auth", format!("Bearer {key}"))]),
            AuthMethod::OidcToken(token) => Ok(vec![("x-fence-auth", format!("Bearer {token}"))]),
            AuthMethod::GatewayKeyFile(path) => {
                let key = std::fs::read_to_string(path).with_context(|| {
                    format!("failed to read gateway key file {}", path.display())
                })?;
                let key = key.trim();
                if key.is_empty() {
                    anyhow::bail!("gateway key file {} is empty", path.display());
                }
                Ok(vec![("x-fence-auth", format!("Bearer {key}"))])
            }
            AuthMethod::GatewayKey(key) => Ok(vec![("x-fence-auth", format!("Bearer {key}"))]),
            AuthMethod::ApiKey(key) => Ok(vec![("x-fence-auth", format!("Bearer {key}"))]),
        }
    }
}

/// Run the local proxy server.
pub async fn run_proxy(config: ProxyConfig) -> Result<()> {
    run_proxy_until_shutdown(config, pending::<()>()).await
}

/// Run the local proxy from synchronous CLI code.
///
/// Actix `HttpServer` requires an Actix system/local runtime. Running it by
/// directly blocking on a plain Tokio multi-thread runtime can panic with
/// `spawn_local called from outside of a task::LocalSet`.
pub fn run_proxy_blocking(config: ProxyConfig) -> Result<()> {
    actix_web::rt::System::new().block_on(run_proxy(config))
}

/// Run the local proxy from synchronous code until the supplied shutdown future resolves.
#[cfg(test)]
fn run_proxy_until_shutdown_blocking<S>(config: ProxyConfig, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    actix_web::rt::System::new().block_on(run_proxy_until_shutdown(config, shutdown))
}

/// Run the local proxy server until the supplied shutdown future resolves.
pub async fn run_proxy_until_shutdown<S>(config: ProxyConfig, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let fence_url = config.fence_url.trim_end_matches('/').to_string();
    let shared = web::Data::new(ProxyState {
        fence_url,
        auth_method: config.auth_method.clone(),
        correlation_headers: config.correlation_headers.clone(),
        local_api_key: config.local_api_key.clone(),
        subscription_mode: config.subscription_mode,
        provider_auth_env_var: config.provider_auth_env_var.clone(),
        protocol_diffs_dir: config.protocol_diffs_dir.clone(),
        observe_request_duration: config.observe_request_duration,
    });

    let addr = format!("127.0.0.1:{}", config.listen_port);
    info!("Starting local proxy on {}", addr);
    info!(
        fence_url = %shared.fence_url,
        auth = match &config.auth_method {
            AuthMethod::MasterKey(_) => "master-key",
            AuthMethod::OidcToken(_) => "OIDC JWT",
            AuthMethod::GatewayKeyFile(_) => "gateway-key-file",
            AuthMethod::GatewayKey(_) => "gateway-key",
            AuthMethod::ApiKey(_) => "api-key",
        },
        "Proxy configuration"
    );
    if config.verbose {
        eprintln!("Local proxy listening on http://{addr}");
        eprintln!("Forwarding to fence at {}", shared.fence_url);
        if !shared.correlation_headers.is_empty() {
            eprintln!("Injecting session correlation headers for backend capture");
        }
        if let Some(ref key) = shared.local_api_key {
            eprintln!();
            eprintln!("Generated local API key: {key}");
            if shared.subscription_mode {
                eprintln!("Subscription mode: also accepting OpenAI access tokens");
            }
        } else if shared.subscription_mode {
            eprintln!();
            eprintln!(
                "Subscription mode: forwarding provider authentication without a local API key"
            );
        }
        if let Some(ref dir) = shared.protocol_diffs_dir {
            eprintln!("Protocol diffs: {}", dir.display());
        }
        eprintln!();
        eprintln!("Configure your CLI tool:");
        print_client_env_commands(config.listen_port);
    }

    let server = HttpServer::new(move || {
        App::new()
            .app_data(shared.clone())
            .default_service(web::to(proxy_handler))
    })
    .bind(&addr)
    .with_context(|| format!("Failed to bind to {addr}"))?
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(async move {
        shutdown.await;
        handle.stop(true).await;
    });
    server.await?;

    Ok(())
}

/// Shared state for the proxy.
struct ProxyState {
    fence_url: String,
    auth_method: AuthMethod,
    correlation_headers: Vec<(String, String)>,
    local_api_key: Option<String>,
    subscription_mode: bool,
    provider_auth_env_var: Option<String>,
    protocol_diffs_dir: Option<PathBuf>,
    observe_request_duration: fn(f64),
}

/// Handle all incoming requests — forward to the fence server with auth injection.
async fn proxy_handler(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<ProxyState>,
) -> Result<HttpResponse, actix_web::Error> {
    let request_start = Instant::now();
    let fence_url = build_fence_url(&state.fence_url, &req);
    let method_str = req.method().as_str();

    // Validate local API key if the proxy was configured with one.
    // Skip validation for root/health paths (agent health checks).
    let path = req.uri().path();
    let is_health_check = path == "/" || path == "/healthz";
    if let Some(ref local_key) = state.local_api_key {
        if !is_health_check {
            let client_auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let is_local_key = headers_present_local_key(req.headers(), local_key);
            let is_subscription_token =
                state.subscription_mode && looks_like_openai_token(client_auth);
            if !is_local_key && !is_subscription_token {
                warn!(path = %req.uri(), "Rejected request with invalid local API key");
                (state.observe_request_duration)(request_start.elapsed().as_secs_f64() * 1000.0);
                return Ok(HttpResponse::Unauthorized().body("Invalid API key"));
            }
        }
    }

    if is_responses_websocket_request(&req) {
        return proxy_websocket(req, payload, state, fence_url).await;
    }

    let body = read_payload(payload).await?;
    let (upstream_body, synthesize_anthropic_json) =
        anthropic_backend_stream_body(&req, body.as_ref())?;

    // Build the upstream request with auth headers injected
    let upstream_method: reqwest::Method = method_str.parse().unwrap_or(reqwest::Method::GET);
    let upstream_client = reqwest::Client::builder()
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::ACCEPT_ENCODING,
            reqwest::header::HeaderValue::from_static("identity"),
        )]))
        .build()
        .map_err(|e| {
            actix_web::error::ErrorInternalServerError(format!("Failed to create HTTP client: {e}"))
        })?;
    let mut upstream_req = upstream_client
        .request(upstream_method, &fence_url)
        .body(upstream_body);
    let provider_auth_token = state.provider_auth_token();

    // Copy relevant headers from the client request
    for (name, value) in req.headers() {
        let name_lower = name.as_str().to_lowercase();
        // Skip hop-by-hop headers and headers we're replacing
        if should_skip_client_header(&name_lower) || name_lower == "accept-encoding" {
            continue;
        }
        if state
            .local_api_key
            .as_deref()
            .is_some_and(|local_key| is_local_api_key_header(&name_lower, value, local_key))
        {
            continue;
        }
        if name_lower == "authorization" && provider_auth_token.is_some() {
            continue;
        }
        if let Ok(val) = value.to_str() {
            upstream_req = upstream_req.header(name.as_str(), val);
        }
    }
    if let Some(token) = provider_auth_token {
        tracing::debug!(path = %req.uri().path(), "Injecting provider Authorization header from proxy env");
        upstream_req = upstream_req.header("authorization", format!("Bearer {token}"));
    }

    // Inject fence auth headers
    let auth_headers = state.auth_method.headers().map_err(|e| {
        warn!(error = %e, "Failed to resolve proxy authentication headers");
        actix_web::error::ErrorInternalServerError(format!(
            "Failed to resolve proxy authentication headers: {e}"
        ))
    })?;
    for (name, value) in auth_headers {
        upstream_req = upstream_req.header(name, value);
    }
    for (name, value) in &state.correlation_headers {
        upstream_req = upstream_req.header(name.as_str(), value.as_str());
    }
    upstream_req = upstream_req
        .header("accept-encoding", "identity")
        .header("x-fence-local-proxy", "true")
        .header("x-fence-stream-keepalive", "true");

    // Inject protocol diffs directory header when configured
    if let Some(ref dir) = state.protocol_diffs_dir {
        upstream_req =
            upstream_req.header("x-fence-protocol-diffs-dir", dir.to_string_lossy().as_ref());
    }

    // Send the request
    let upstream_resp = upstream_req.send().await.map_err(|e| {
        warn!(error = %e, "Failed to forward request to fence");
        actix_web::error::ErrorInternalServerError(format!("Upstream request failed: {e}"))
    })?;

    let status_u16 = upstream_resp.status().as_u16();
    let status = ActixStatus::from_u16(status_u16).unwrap_or(ActixStatus::INTERNAL_SERVER_ERROR);
    (state.observe_request_duration)(request_start.elapsed().as_secs_f64() * 1000.0);

    // Build the response
    let mut builder = HttpResponse::build(status);

    // Copy response headers
    for (name, value) in upstream_resp.headers() {
        let name_lower = name.as_str().to_lowercase();
        if matches!(
            name_lower.as_str(),
            "transfer-encoding" | "connection" | "keep-alive"
        ) {
            continue;
        }
        if synthesize_anthropic_json && name_lower == "content-type" {
            continue;
        }
        if let Ok(val) = value.to_str() {
            builder.insert_header((name.as_str(), val));
        }
    }

    // Check if this is a streaming response (SSE or chunked)
    let content_type = upstream_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if synthesize_anthropic_json && content_type.contains("text/event-stream") {
        let stream_body = upstream_resp
            .bytes()
            .await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
        let body = synthesize_anthropic_message_json(&stream_body)?;
        builder.insert_header(("content-type", "application/json"));
        Ok(builder.body(body))
    } else if content_type.contains("text/event-stream") || content_type.contains("text/plain") {
        // SSE streaming — forward as a streamed response body
        let stream = upstream_resp.bytes_stream().map(|result| {
            result.map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))
        });
        Ok(builder.streaming(stream))
    } else {
        // Regular response — buffer and return
        let resp_body = upstream_resp
            .bytes()
            .await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
        Ok(builder.body(resp_body))
    }
}

async fn read_payload(mut payload: web::Payload) -> Result<web::Bytes, actix_web::Error> {
    let mut body = BytesMut::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(actix_web::error::ErrorBadRequest)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn anthropic_backend_stream_body(
    req: &HttpRequest,
    body: &[u8],
) -> Result<(Vec<u8>, bool), actix_web::Error> {
    if req.method() != actix_web::http::Method::POST || !is_anthropic_messages_path(req.path()) {
        return Ok((body.to_vec(), false));
    }

    let mut value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| actix_web::error::ErrorBadRequest(format!("Invalid Anthropic JSON: {e}")))?;
    let original_stream = value
        .get("stream")
        .and_then(|stream| stream.as_bool())
        .unwrap_or(false);
    let Some(object) = value.as_object_mut() else {
        return Ok((body.to_vec(), false));
    };

    object.insert("stream".to_string(), serde_json::Value::Bool(true));
    let upstream_body = serde_json::to_vec(&value)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    Ok((upstream_body, !original_stream))
}

fn is_anthropic_messages_path(path: &str) -> bool {
    matches!(
        path,
        "/messages" | "/v1/messages" | "/api/v1/anthropic/messages"
    )
}

fn synthesize_anthropic_message_json(stream_body: &[u8]) -> Result<Vec<u8>, actix_web::Error> {
    let stream_text = String::from_utf8_lossy(stream_body);
    let mut message = serde_json::Map::new();
    let mut content = Vec::<serde_json::Value>::new();
    let mut stop_reason: Option<serde_json::Value> = None;
    let mut stop_sequence: Option<serde_json::Value> = None;
    let mut usage = serde_json::json!({"input_tokens": 0, "output_tokens": 0});
    let mut stream_error: Option<serde_json::Value> = None;

    for frame in stream_text.split("\n\n") {
        let mut event_type = "";
        let mut data = String::new();
        for line in frame.lines() {
            if line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if event_type == "error" || value.get("type").and_then(|v| v.as_str()) == Some("error") {
            stream_error = Some(value);
            continue;
        }

        match value.get("type").and_then(|v| v.as_str()) {
            Some("message_start") => {
                if let Some(start_message) = value.get("message").and_then(|v| v.as_object()) {
                    for key in ["id", "type", "role", "model"] {
                        if let Some(field) = start_message.get(key) {
                            message.insert(key.to_string(), field.clone());
                        }
                    }
                    if let Some(start_usage) = start_message.get("usage") {
                        usage = start_usage.clone();
                    }
                }
            }
            Some("content_block_start") => {
                let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                ensure_content_index(&mut content, index);
                if let Some(block) = value.get("content_block") {
                    content[index] = block.clone();
                }
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                ensure_content_index(&mut content, index);
                apply_anthropic_delta(&mut content[index], value.get("delta"));
            }
            Some("message_delta") => {
                if let Some(delta) = value.get("delta") {
                    if let Some(reason) = delta.get("stop_reason") {
                        stop_reason = Some(reason.clone());
                    }
                    if let Some(sequence) = delta.get("stop_sequence") {
                        stop_sequence = Some(sequence.clone());
                    }
                }
                if let Some(delta_usage) = value.get("usage").and_then(|v| v.as_object()) {
                    merge_usage(&mut usage, delta_usage);
                }
            }
            _ => {}
        }
    }

    if let Some(error) = stream_error {
        return serde_json::to_vec(&error)
            .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()));
    }

    message
        .entry("id".to_string())
        .or_insert_with(|| serde_json::Value::String("msg_ai_fence_stream".to_string()));
    message
        .entry("type".to_string())
        .or_insert_with(|| serde_json::Value::String("message".to_string()));
    message
        .entry("role".to_string())
        .or_insert_with(|| serde_json::Value::String("assistant".to_string()));
    message.insert("content".to_string(), serde_json::Value::Array(content));
    message.insert(
        "stop_reason".to_string(),
        stop_reason.unwrap_or(serde_json::Value::Null),
    );
    message.insert(
        "stop_sequence".to_string(),
        stop_sequence.unwrap_or(serde_json::Value::Null),
    );
    message.insert("usage".to_string(), usage);

    serde_json::to_vec(&serde_json::Value::Object(message))
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))
}

fn ensure_content_index(content: &mut Vec<serde_json::Value>, index: usize) {
    while content.len() <= index {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }
}

fn apply_anthropic_delta(block: &mut serde_json::Value, delta: Option<&serde_json::Value>) {
    let Some(delta) = delta else {
        return;
    };
    match delta.get("type").and_then(|v| v.as_str()) {
        Some("text_delta") => append_string_field(block, "text", delta.get("text")),
        Some("thinking_delta") => append_string_field(block, "thinking", delta.get("thinking")),
        Some("signature_delta") => append_string_field(block, "signature", delta.get("signature")),
        Some("input_json_delta") => {
            append_string_field(block, "partial_json", delta.get("partial_json"))
        }
        _ => {}
    }
}

fn append_string_field(
    object: &mut serde_json::Value,
    field: &str,
    addition: Option<&serde_json::Value>,
) {
    let Some(addition) = addition.and_then(|value| value.as_str()) else {
        return;
    };
    if let Some(map) = object.as_object_mut() {
        let current = map
            .get(field)
            .and_then(|value| value.as_str())
            .unwrap_or("");
        map.insert(
            field.to_string(),
            serde_json::Value::String(format!("{current}{addition}")),
        );
    }
}

fn merge_usage(
    usage: &mut serde_json::Value,
    delta_usage: &serde_json::Map<String, serde_json::Value>,
) {
    let Some(usage_obj) = usage.as_object_mut() else {
        return;
    };
    for (key, value) in delta_usage {
        usage_obj.insert(key.clone(), value.clone());
    }
}

async fn proxy_websocket(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<ProxyState>,
    fence_url: String,
) -> Result<HttpResponse, actix_web::Error> {
    let (mut response, mut client_session, client_stream) = actix_ws::handle(&req, payload)?;
    add_codex_websocket_headers(&mut response);
    let mut client_stream = client_stream
        .max_frame_size(LOCAL_PROXY_MAX_WEBSOCKET_MESSAGE_BYTES)
        .aggregate_continuations()
        .max_continuation_size(LOCAL_PROXY_MAX_WEBSOCKET_MESSAGE_BYTES);

    let upstream_request = build_upstream_websocket_request(&req, &state, &fence_url)?;
    actix_web::rt::spawn(async move {
        let (mut upstream_sink, mut upstream_stream) = match connect_local_proxy_upstream_websocket(
            upstream_request.clone(),
            state.observe_request_duration,
        )
        .await
        {
            Ok(ws) => {
                let (sink, stream) = ws.split();
                (Some(sink), Some(stream))
            }
            Err(err) => {
                warn!(error = %err, "Failed to connect upstream WebSocket");
                send_local_proxy_websocket_connect_error(&mut client_session, &err).await;
                return;
            }
        };
        let mut upstream_in_flight = false;
        loop {
            tokio::select! {
                client_msg = client_stream.next() => {
                    let Some(client_msg) = client_msg else {
                        tracing::debug!("Local WebSocket client stream ended; closing upstream WebSocket");
                        if let Some(upstream_sink) = upstream_sink.as_mut() {
                            let _ = upstream_sink.send(tungstenite::Message::Close(None)).await;
                        }
                        break;
                    };
                    let client_msg = match client_msg {
                        Ok(msg) => msg,
                        Err(err) => {
                            if is_actix_websocket_eof_error(&err) {
                                tracing::debug!(error = %err, "Local WebSocket client disconnected without close frame; closing upstream WebSocket");
                            } else {
                                warn!(error = %err, "Local WebSocket client read failed; closing upstream WebSocket");
                            }
                            if let Some(upstream_sink) = upstream_sink.as_mut() {
                                let _ = upstream_sink.send(tungstenite::Message::Close(None)).await;
                            }
                            break;
                        }
                    };
                    let sent_message = match client_ws_message_to_upstream(client_msg, &mut client_session).await {
                        ClientWebSocketMessage::Forward(message) => message,
                        ClientWebSocketMessage::Close(reason) => {
                            if let Some(upstream_sink) = upstream_sink.as_mut() {
                                let _ = upstream_sink.send(tungstenite::Message::Close(reason)).await;
                            }
                            break;
                        }
                    };

                    if upstream_sink.is_none() {
                        tracing::info!("Reconnecting local proxy upstream WebSocket before forwarding client frame");
                        match connect_local_proxy_upstream_websocket(
                            upstream_request.clone(),
                            state.observe_request_duration,
                        )
                        .await
                        {
                            Ok(ws) => {
                                let (sink, stream) = ws.split();
                                upstream_sink = Some(sink);
                                upstream_stream = Some(stream);
                            }
                            Err(err) => {
                                warn!(error = %err, "Failed to reconnect upstream WebSocket");
                                send_local_proxy_websocket_connect_error(&mut client_session, &err).await;
                                break;
                            }
                        }
                    }

                    let is_request_payload = matches!(
                        sent_message,
                        tungstenite::Message::Text(_) | tungstenite::Message::Binary(_)
                    );
                    let send_result = upstream_sink
                        .as_mut()
                        .expect("checked upstream connection")
                        .send(sent_message.clone())
                        .await;
                    if let Err(err) = send_result {
                        warn!(error = %err, "Local proxy upstream WebSocket send failed; reconnecting once");
                        match connect_local_proxy_upstream_websocket(
                            upstream_request.clone(),
                            state.observe_request_duration,
                        )
                        .await
                        {
                            Ok(ws) => {
                                let (mut sink, stream) = ws.split();
                                if let Err(err) = sink.send(sent_message).await {
                                    warn!(error = %err, "Local proxy upstream WebSocket resend failed after reconnect");
                                    let _ = client_session.close(None).await;
                                    break;
                                }
                                upstream_sink = Some(sink);
                                upstream_stream = Some(stream);
                            }
                            Err(err) => {
                                warn!(error = %err, "Failed to reconnect upstream WebSocket after send failure");
                                send_local_proxy_websocket_connect_error(&mut client_session, &err).await;
                                break;
                            }
                        }
                    }
                    if is_request_payload {
                        upstream_in_flight = true;
                    }
                }
                upstream_msg = async {
                    match upstream_stream.as_mut() {
                        Some(stream) => stream.next().await,
                        None => pending().await,
                    }
                } => {
                    let Some(upstream_msg) = upstream_msg else {
                        tracing::debug!(in_flight = upstream_in_flight, "Upstream WebSocket stream ended");
                        upstream_sink = None;
                        upstream_stream = None;
                        if upstream_in_flight {
                            let _ = client_session.close(None).await;
                            break;
                        }
                        continue;
                    };
                    let upstream_msg = match upstream_msg {
                        Ok(msg) => msg,
                        Err(err) => {
                            upstream_sink = None;
                            upstream_stream = None;
                            if is_tungstenite_websocket_eof_error(&err) {
                                tracing::debug!(error = %err, in_flight = upstream_in_flight, "Upstream WebSocket disconnected without close frame");
                            } else {
                                warn!(error = %err, in_flight = upstream_in_flight, "Upstream WebSocket read failed");
                            }
                            if upstream_in_flight {
                                let _ = client_session.close(None).await;
                                break;
                            }
                            continue;
                        }
                    };
                    let upstream_finished = local_proxy_upstream_message_finishes(&upstream_msg);
                    match upstream_msg {
                        tungstenite::Message::Close(reason) if !upstream_in_flight => {
                            tracing::debug!(?reason, "Idle upstream WebSocket closed; keeping local WebSocket open");
                            upstream_sink = None;
                            upstream_stream = None;
                            continue;
                        }
                        message => {
                            if !forward_upstream_ws_message(message, &mut client_session).await {
                                break;
                            }
                        }
                    }
                    if upstream_finished {
                        upstream_in_flight = false;
                    }
                }
                _ = sleep(LOCAL_PROXY_UPSTREAM_IDLE_PING_INTERVAL), if upstream_sink.is_some() && !upstream_in_flight => {
                    let ping_result = upstream_sink
                        .as_mut()
                        .expect("checked upstream connection")
                        .send(tungstenite::Message::Ping(BytesMut::from("ai-fence-idle").freeze()))
                        .await;
                    if let Err(err) = ping_result {
                        tracing::debug!(error = %err, "Local proxy upstream WebSocket idle ping failed");
                        upstream_sink = None;
                        upstream_stream = None;
                    }
                }
            }
        }
    });

    Ok(response)
}

async fn connect_local_proxy_upstream_websocket(
    request: tungstenite::handshake::client::Request,
    observe_request_duration: fn(f64),
) -> Result<LocalProxyUpstreamWebSocket, Box<tungstenite::Error>> {
    let connect_start = Instant::now();
    let mut last_error = None;
    for attempt in 1..=LOCAL_PROXY_WEBSOCKET_CONNECT_ATTEMPTS {
        match connect_async(request.clone()).await {
            Ok((ws, _)) => {
                observe_request_duration(connect_start.elapsed().as_secs_f64() * 1000.0);
                if attempt > 1 {
                    tracing::info!(attempt, "Local proxy upstream WebSocket reconnected");
                }
                return Ok(ws);
            }
            Err(err) => {
                warn!(
                    attempt,
                    max_attempts = LOCAL_PROXY_WEBSOCKET_CONNECT_ATTEMPTS,
                    error = %err,
                    "Local proxy upstream WebSocket connect attempt failed"
                );
                last_error = Some(err);
                if attempt < LOCAL_PROXY_WEBSOCKET_CONNECT_ATTEMPTS {
                    sleep(LOCAL_PROXY_WEBSOCKET_CONNECT_BACKOFF * attempt as u32).await;
                }
            }
        }
    }
    Err(Box::new(last_error.expect("at least one connect attempt")))
}

async fn send_local_proxy_websocket_connect_error(
    client_session: &mut actix_ws::Session,
    err: &tungstenite::Error,
) {
    let _ = client_session
        .text(
            serde_json::json!({
                "type": "error",
                "status": 502,
                "error": {
                    "type": "server_error",
                    "message": format!("Upstream WebSocket connection failed: {err}"),
                    "code": "server_error"
                }
            })
            .to_string(),
        )
        .await;
    let _ = client_session.clone().close(None).await;
}

enum ClientWebSocketMessage {
    Forward(tungstenite::Message),
    Close(Option<tungstenite::protocol::CloseFrame>),
}

async fn client_ws_message_to_upstream(
    msg: AggregatedMessage,
    client_session: &mut actix_ws::Session,
) -> ClientWebSocketMessage {
    match msg {
        AggregatedMessage::Text(text) => {
            ClientWebSocketMessage::Forward(tungstenite::Message::Text(text.to_string().into()))
        }
        AggregatedMessage::Binary(bytes) => {
            ClientWebSocketMessage::Forward(tungstenite::Message::Binary(bytes.to_vec().into()))
        }
        AggregatedMessage::Ping(bytes) => {
            let _ = client_session.pong(&bytes).await;
            ClientWebSocketMessage::Forward(tungstenite::Message::Ping(bytes.to_vec().into()))
        }
        AggregatedMessage::Pong(bytes) => {
            ClientWebSocketMessage::Forward(tungstenite::Message::Pong(bytes.to_vec().into()))
        }
        AggregatedMessage::Close(reason) => {
            tracing::debug!(?reason, "Local WebSocket client sent close frame");
            let upstream_reason = reason.clone().map(actix_close_to_tungstenite);
            let _ = client_session.clone().close(reason).await;
            ClientWebSocketMessage::Close(upstream_reason)
        }
    }
}

fn local_proxy_upstream_message_finishes(msg: &tungstenite::Message) -> bool {
    let tungstenite::Message::Text(text) = msg else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|event_type| {
            matches!(
                event_type.as_str(),
                "response.completed" | "response.failed"
            )
        })
}

fn is_actix_websocket_eof_error(error: &actix_ws::ProtocolError) -> bool {
    error
        .to_string()
        .contains("payload reached EOF before completing")
}

fn is_tungstenite_websocket_eof_error(error: &tungstenite::Error) -> bool {
    matches!(
        error,
        tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
        )
    )
}

fn build_upstream_websocket_request(
    req: &HttpRequest,
    state: &ProxyState,
    fence_url: &str,
) -> Result<tungstenite::handshake::client::Request, actix_web::Error> {
    let ws_url = websocket_url(fence_url).map_err(actix_web::error::ErrorInternalServerError)?;
    let mut upstream_req = ws_url
        .into_client_request()
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let provider_auth_token = state.provider_auth_token();

    for (name, value) in req.headers() {
        let name_lower = name.as_str().to_lowercase();
        if should_skip_client_header(&name_lower) || is_websocket_handshake_header(&name_lower) {
            continue;
        }
        if state
            .local_api_key
            .as_deref()
            .is_some_and(|local_key| is_local_api_key_header(&name_lower, value, local_key))
        {
            continue;
        }
        if name_lower == "authorization" && provider_auth_token.is_some() {
            continue;
        }
        if let Ok(val) = value.to_str() {
            let header_name = tungstenite::http::HeaderName::from_bytes(name.as_str().as_bytes())
                .map_err(actix_web::error::ErrorBadRequest)?;
            let header_value = tungstenite::http::HeaderValue::from_str(val)
                .map_err(actix_web::error::ErrorBadRequest)?;
            upstream_req.headers_mut().insert(header_name, header_value);
        }
    }
    if let Some(token) = provider_auth_token {
        tracing::debug!(path = %req.uri().path(), "Injecting provider Authorization header from proxy env for WebSocket");
        upstream_req.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(actix_web::error::ErrorInternalServerError)?,
        );
    }

    let auth_headers = state.auth_method.headers().map_err(|e| {
        warn!(error = %e, "Failed to resolve proxy authentication headers");
        actix_web::error::ErrorInternalServerError(format!(
            "Failed to resolve proxy authentication headers: {e}"
        ))
    })?;
    for (name, value) in auth_headers {
        upstream_req.headers_mut().insert(
            name,
            value
                .parse()
                .map_err(actix_web::error::ErrorInternalServerError)?,
        );
    }
    for (name, value) in &state.correlation_headers {
        let header_name = tungstenite::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let header_value = tungstenite::http::HeaderValue::from_str(value)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        upstream_req.headers_mut().insert(header_name, header_value);
    }
    upstream_req.headers_mut().insert(
        "x-fence-local-proxy",
        tungstenite::http::HeaderValue::from_static("true"),
    );
    if let Some(ref dir) = state.protocol_diffs_dir {
        upstream_req.headers_mut().insert(
            "x-fence-protocol-diffs-dir",
            dir.to_string_lossy()
                .parse()
                .map_err(actix_web::error::ErrorInternalServerError)?,
        );
    }

    Ok(upstream_req)
}

async fn forward_upstream_ws_message(
    msg: tungstenite::Message,
    client_session: &mut actix_ws::Session,
) -> bool {
    match msg {
        tungstenite::Message::Text(text) => client_session.text(text.to_string()).await.is_ok(),
        tungstenite::Message::Binary(bytes) => client_session.binary(bytes.to_vec()).await.is_ok(),
        tungstenite::Message::Ping(bytes) => client_session.ping(&bytes).await.is_ok(),
        tungstenite::Message::Pong(bytes) => client_session.pong(&bytes).await.is_ok(),
        tungstenite::Message::Close(reason) => {
            let _ = client_session
                .clone()
                .close(reason.map(tungstenite_close_to_actix))
                .await;
            false
        }
        tungstenite::Message::Frame(_) => true,
    }
}

fn tungstenite_close_to_actix(reason: tungstenite::protocol::CloseFrame) -> actix_ws::CloseReason {
    actix_ws::CloseReason {
        code: actix_ws::CloseCode::from(u16::from(reason.code)),
        description: if reason.reason.is_empty() {
            None
        } else {
            Some(reason.reason.to_string())
        },
    }
}

fn actix_close_to_tungstenite(reason: actix_ws::CloseReason) -> tungstenite::protocol::CloseFrame {
    tungstenite::protocol::CloseFrame {
        code: tungstenite::protocol::frame::coding::CloseCode::from(u16::from(reason.code)),
        reason: reason.description.unwrap_or_default().into(),
    }
}

fn is_responses_websocket_request(req: &HttpRequest) -> bool {
    is_websocket_upgrade(req)
        && matches!(
            req.path(),
            "/v1/responses" | "/api/v1/responses" | "/responses"
        )
}

fn is_websocket_upgrade(req: &HttpRequest) -> bool {
    let has_upgrade_connection = req
        .headers()
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        });
    let is_websocket = req
        .headers()
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    has_upgrade_connection && is_websocket
}

fn websocket_url(http_url: &str) -> Result<String> {
    if let Some(rest) = http_url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if let Some(rest) = http_url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else {
        anyhow::bail!("cannot convert URL to WebSocket URL: {http_url}")
    }
}

fn add_codex_websocket_headers(response: &mut HttpResponse) {
    use actix_web::http::header::{HeaderName, HeaderValue};
    response.headers_mut().insert(
        HeaderName::from_static("x-reasoning-included"),
        HeaderValue::from_static(""),
    );
}

/// Build the fence URL from the local request path and query.
fn build_fence_url(fence_url: &str, req: &HttpRequest) -> String {
    let mut url = format!("{}{}", fence_url, req.path());
    let qs = req.query_string();
    if !qs.is_empty() {
        url.push('?');
        url.push_str(qs);
    }
    url
}

fn should_skip_client_header(name_lower: &str) -> bool {
    if name_lower.starts_with("x-fence-") {
        return true;
    }
    matches!(
        name_lower,
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "x-session-id"
            | "x-conversation-id"
            | "x-execution-id"
            | "x-workspace-id"
            | "x-target-id"
    )
}

fn headers_present_local_key(headers: &HeaderMap, local_key: &str) -> bool {
    headers
        .get("authorization")
        .is_some_and(|value| header_is_local_bearer(value, local_key))
        || headers
            .get("x-api-key")
            .is_some_and(|value| header_value_equals(value, local_key))
}

fn is_local_api_key_header(
    name_lower: &str,
    value: &actix_web::http::header::HeaderValue,
    local_key: &str,
) -> bool {
    match name_lower {
        "authorization" => header_is_local_bearer(value, local_key),
        "x-api-key" => header_value_equals(value, local_key),
        _ => false,
    }
}

fn header_is_local_bearer(value: &actix_web::http::header::HeaderValue, local_key: &str) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == local_key)
}

fn header_value_equals(value: &actix_web::http::header::HeaderValue, expected: &str) -> bool {
    value.to_str().ok().is_some_and(|value| value == expected)
}

impl ProxyState {
    fn provider_auth_token(&self) -> Option<String> {
        self.provider_auth_env_var
            .as_deref()
            .and_then(|env_var| std::env::var(env_var).ok())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }
}

fn is_websocket_handshake_header(name_lower: &str) -> bool {
    matches!(
        name_lower,
        "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "sec-websocket-extensions"
    )
}

/// Heuristic to detect an OpenAI access token (JWT or sk-/sess- prefix).
fn looks_like_openai_token(auth_header: &str) -> bool {
    let token = auth_header.strip_prefix("Bearer ").unwrap_or(auth_header);
    token.starts_with("sk-") || token.starts_with("sess-") || token.split('.').count() == 3
}

fn print_client_env_commands(port: u16) {
    let mut stderr = std::io::stderr().lock();
    let _ = write_client_env_commands(&mut stderr, port);
}

fn write_client_env_commands(mut output: impl std::io::Write, port: u16) -> std::io::Result<()> {
    if cfg!(windows) {
        writeln!(output, "  PowerShell:")?;
        writeln!(
            output,
            "    $env:OPENAI_BASE_URL = \"http://127.0.0.1:{port}/v1\""
        )?;
        writeln!(
            output,
            "    $env:ANTHROPIC_BASE_URL = \"http://127.0.0.1:{port}\""
        )?;
        writeln!(output, "  cmd.exe:")?;
        writeln!(output, "    set OPENAI_BASE_URL=http://127.0.0.1:{port}/v1")?;
        writeln!(output, "    set ANTHROPIC_BASE_URL=http://127.0.0.1:{port}")?;
    } else {
        writeln!(
            output,
            "  export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1"
        )?;
        writeln!(
            output,
            "  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}"
        )?;
    }
    Ok(())
}

pub fn correlation_headers(
    session_id: Option<String>,
    conversation_id: Option<String>,
    execution_id: Option<String>,
    workspace_id: Option<String>,
    target_id: Option<String>,
) -> Vec<(String, String)> {
    [
        ("x-session-id", session_id),
        ("x-conversation-id", conversation_id),
        ("x-execution-id", execution_id),
        ("x-workspace-id", workspace_id),
        ("x-target-id", target_id),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (name.to_string(), value))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::web;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    static NEXT_PROXY_TEST_PORT: AtomicU16 = AtomicU16::new(32000);

    fn allocate_proxy_test_port() -> u16 {
        for _ in 0..1000 {
            let port = NEXT_PROXY_TEST_PORT.fetch_add(1, Ordering::SeqCst);
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return port;
            }
        }
        TcpListener::bind("127.0.0.1:0")
            .expect("bind proxy port")
            .local_addr()
            .expect("proxy addr")
            .port()
    }

    #[test]
    fn proxy_strips_untrusted_auth_and_correlation_headers() {
        assert!(should_skip_client_header("x-fence-auth"));
        assert!(should_skip_client_header("x-fence-auth"));
        assert!(should_skip_client_header("x-fence-api-key"));
        assert!(should_skip_client_header("x-fence-protocol-diffs-dir"));
        assert!(should_skip_client_header("x-fence-anything"));
        assert!(should_skip_client_header("x-session-id"));
        assert!(should_skip_client_header("x-conversation-id"));
        assert!(should_skip_client_header("x-execution-id"));
        assert!(should_skip_client_header("x-workspace-id"));
        assert!(should_skip_client_header("x-target-id"));
        assert!(!should_skip_client_header("authorization"));
        assert!(!should_skip_client_header("content-type"));
    }

    #[test]
    fn proxy_builds_only_non_empty_correlation_headers() {
        let headers = correlation_headers(
            Some("sess_1".to_string()),
            Some(" ".to_string()),
            None,
            Some("ws_1".to_string()),
            None,
        );
        assert_eq!(
            headers,
            vec![
                ("x-session-id".to_string(), "sess_1".to_string()),
                ("x-workspace-id".to_string(), "ws_1".to_string()),
            ]
        );
    }

    #[test]
    fn proxy_env_commands_are_printable() {
        print_client_env_commands(8181);
    }

    #[test]
    fn proxy_recognizes_responses_websocket_upgrade() {
        let req = actix_web::test::TestRequest::get()
            .uri("/v1/responses")
            .insert_header(("connection", "keep-alive, Upgrade"))
            .insert_header(("upgrade", "websocket"))
            .to_http_request();
        assert!(is_responses_websocket_request(&req));

        let non_responses = actix_web::test::TestRequest::get()
            .uri("/v1/models")
            .insert_header(("connection", "Upgrade"))
            .insert_header(("upgrade", "websocket"))
            .to_http_request();
        assert!(!is_responses_websocket_request(&non_responses));
    }

    #[test]
    fn proxy_converts_http_urls_to_websocket_urls() {
        assert_eq!(
            websocket_url("http://127.0.0.1:8080/v1/responses").unwrap(),
            "ws://127.0.0.1:8080/v1/responses"
        );
        assert_eq!(
            websocket_url("https://fence.example.com/v1/responses").unwrap(),
            "wss://fence.example.com/v1/responses"
        );
        assert!(websocket_url("ftp://example.com").is_err());
    }

    #[actix_web::test]
    async fn proxy_handler_uses_configured_state_data() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 2048];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth: Bearer test-master"));
            assert!(request.contains("x-session-id: sess_1"));
            assert!(request.contains("x-fence-local-proxy: true"));
            assert!(request.contains("x-fence-stream-keepalive: true"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::MasterKey("test-master".to_string()),
            correlation_headers: vec![("x-session-id".to_string(), "sess_1".to_string())],
            local_api_key: None,
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/responses")
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_forces_anthropic_backend_stream_and_synthesizes_json() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("POST /v1/messages"));
            assert!(request.contains("\"stream\":true"));
            assert!(request.contains("x-fence-stream-keepalive: true"));

            let body = concat!(
                ": ai-fence keepalive\n\n",
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello world\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::MasterKey("test-master".to_string()),
            correlation_headers: Vec::new(),
            local_api_key: None,
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/messages")
            .insert_header(("content-type", "application/json"))
            .set_payload(
                serde_json::json!({
                    "model": "claude-sonnet-4-20250514",
                    "max_tokens": 100,
                    "messages": [{"role": "user", "content": "Hello"}]
                })
                .to_string(),
            )
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        let body = actix_web::test::read_body(resp).await;
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["id"], "msg_stream");
        assert_eq!(body["content"][0]["text"], "Hello world");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 10);
        assert_eq!(body["usage"]["output_tokens"], 2);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_gateway_key_file_preserves_provider_authorization() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key_path = temp.path().join("gateway-key");
        std::fs::write(&key_path, "gw_live_session\n").expect("write gateway key");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth: Bearer gw_live_session"));
            assert!(request.contains("authorization: Bearer provider-token"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::GatewayKeyFile(key_path),
            correlation_headers: Vec::new(),
            local_api_key: None,
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/responses")
            .insert_header(("authorization", "Bearer provider-token"))
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_strips_local_bearer_key_before_forwarding() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth:"));
            assert!(!request.contains("authorization: AFR_TOKEN_0542"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::GatewayKey("gw_live_session".to_string()),
            correlation_headers: Vec::new(),
            local_api_key: Some("local-only".to_string()),
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/responses")
            .insert_header(("authorization", format!("{}{}", "Bear", "er local-only")))
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_strips_local_x_api_key_before_forwarding() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth:"));
            assert!(!request.contains("x-api-key: local-only"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::GatewayKey("gw_live_session".to_string()),
            correlation_headers: Vec::new(),
            local_api_key: Some("local-only".to_string()),
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/messages")
            .insert_header(("x-api-key", "local-only"))
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_preserves_subscription_authorization_in_subscription_mode() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth:"));
            assert!(request.contains("authorization:") && request.contains("sess-live-token"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::GatewayKey("gw_live_session".to_string()),
            correlation_headers: Vec::new(),
            local_api_key: Some("local-only".to_string()),
            subscription_mode: true,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/responses")
            .insert_header((
                "authorization",
                format!("{}{}", "Bear", "er sess-live-token"),
            ))
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
    }

    #[actix_web::test]
    async fn proxy_injects_provider_authorization_from_env() {
        std::env::set_var("AI_FENCE_PROXY_TEST_PROVIDER_TOKEN", "provider-token");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("x-fence-auth: Bearer test-master"));
            assert!(request.contains("authorization: Bearer provider-token"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });
        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::MasterKey("test-master".to_string()),
            correlation_headers: Vec::new(),
            local_api_key: None,
            subscription_mode: true,
            provider_auth_env_var: Some("AI_FENCE_PROXY_TEST_PROVIDER_TOKEN".to_string()),
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(state)
                .default_service(web::to(proxy_handler)),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/v1/responses")
            .set_payload("{}")
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), ActixStatus::OK);
        server.join().expect("mock upstream should complete");
        std::env::remove_var("AI_FENCE_PROXY_TEST_PROVIDER_TOKEN");
    }

    #[actix_web::test]
    async fn proxy_sends_idle_ping_between_responses_websocket_turns() {
        async fn ping_aware_upstream(
            req: HttpRequest,
            payload: web::Payload,
            attempts: web::Data<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
            events: web::Data<std::sync::mpsc::Sender<String>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            attempts.fetch_add(1, Ordering::SeqCst);
            let events = events.get_ref().clone();
            actix_web::rt::spawn(async move {
                let mut stream = stream.aggregate_continuations();
                let mut text_count = 0;
                while let Some(Ok(message)) = stream.next().await {
                    match message {
                        AggregatedMessage::Text(text) => {
                            text_count += 1;
                            let _ = events.send(format!("text:{text}"));
                            let _ = session
                                .text(
                                    serde_json::json!({
                                        "type": "response.completed",
                                        "response": {"id": format!("resp_{text_count}")}
                                    })
                                    .to_string(),
                                )
                                .await;
                            if text_count >= 2 {
                                break;
                            }
                        }
                        AggregatedMessage::Ping(bytes) => {
                            let _ = events.send("ping".to_string());
                            let _ = session.pong(&bytes).await;
                        }
                        AggregatedMessage::Close(reason) => {
                            let _ = session.close(reason).await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
            Ok(response)
        }

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (events_tx, events_rx) = mpsc::channel::<String>();
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let attempts_for_server = attempts.clone();
        let events_for_server = events_tx.clone();
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(attempts_for_server.clone()))
                .app_data(web::Data::new(events_for_server.clone()))
                .route("/v1/responses", web::get().to(ping_aware_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let proxy_port = allocate_proxy_test_port();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal proxy starting");
            run_proxy_until_shutdown_blocking(
                ProxyConfig {
                    fence_url: upstream,
                    auth_method: AuthMethod::MasterKey("test-master".to_string()),
                    listen_port: proxy_port,
                    correlation_headers: Vec::new(),
                    local_api_key: None,
                    subscription_mode: false,
                    provider_auth_env_var: None,
                    protocol_diffs_dir: None,
                    verbose: false,
                    observe_request_duration: |_| {},
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .expect("proxy exits cleanly");
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy thread should start");

        let mut last_err = None;
        let (mut ws, _) = 'connect: {
            for _ in 0..20 {
                match tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{proxy_port}/v1/responses"
                ))
                .await
                {
                    Ok(value) => break 'connect value,
                    Err(err) => {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            panic!("proxy did not accept websocket requests: {last_err:?}");
        };

        ws.send(TungsteniteMessage::Text("first".into()))
            .await
            .expect("first client frame should send");
        let _ = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for first completion")
            .expect("first response frame should arrive")
            .expect("first response frame should read");

        let mut saw_ping = false;
        for _ in 0..10 {
            let event = events_rx
                .recv_timeout(Duration::from_millis(200))
                .expect("upstream should receive text or idle ping");
            if event == "ping" {
                saw_ping = true;
                break;
            }
        }
        assert!(saw_ping, "proxy should ping idle upstream WebSocket");

        ws.send(TungsteniteMessage::Text("second".into()))
            .await
            .expect("second client frame should send on same local WebSocket");
        let _ = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for second completion")
            .expect("second response frame should arrive")
            .expect("second response frame should read");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "idle keepalive should avoid reconnecting the upstream WebSocket"
        );

        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_reconnects_after_idle_upstream_websocket_close() {
        async fn reconnecting_upstream(
            req: HttpRequest,
            payload: web::Payload,
            attempts: web::Data<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            actix_web::rt::spawn(async move {
                if attempt == 0 {
                    let _ = session
                        .close(Some(actix_ws::CloseReason {
                            code: actix_ws::CloseCode::Normal,
                            description: Some("idle close".to_string()),
                        }))
                        .await;
                    return;
                }
                let mut stream = stream.aggregate_continuations();
                if let Some(Ok(AggregatedMessage::Text(text))) = stream.next().await {
                    let _ = session.text(text.to_string()).await;
                }
            });
            Ok(response)
        }

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let attempts_for_server = attempts.clone();
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(attempts_for_server.clone()))
                .route("/v1/responses", web::get().to(reconnecting_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let proxy_port = allocate_proxy_test_port();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal proxy starting");
            run_proxy_until_shutdown_blocking(
                ProxyConfig {
                    fence_url: upstream,
                    auth_method: AuthMethod::MasterKey("test-master".to_string()),
                    listen_port: proxy_port,
                    correlation_headers: Vec::new(),
                    local_api_key: None,
                    subscription_mode: false,
                    provider_auth_env_var: None,
                    protocol_diffs_dir: None,
                    verbose: false,
                    observe_request_duration: |_| {},
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .expect("proxy exits cleanly");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy thread should start");

        let mut last_err = None;
        let (mut ws, _) = 'connect: {
            for _ in 0..20 {
                match tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{proxy_port}/v1/responses"
                ))
                .await
                {
                    Ok(value) => break 'connect value,
                    Err(err) => {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            panic!("proxy did not accept websocket requests: {last_err:?}");
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        ws.send(TungsteniteMessage::Text("after-reconnect".into()))
            .await
            .expect("client send should succeed after idle upstream close");
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for echoed frame")
            .expect("websocket stream should yield a frame")
            .expect("websocket read should succeed");

        match msg {
            TungsteniteMessage::Text(text) => assert_eq!(text, "after-reconnect"),
            other => panic!("expected echoed websocket text frame, got {other:?}"),
        }
        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "proxy should reconnect upstream after idle close"
        );

        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_reconnects_after_idle_upstream_drop_without_close_frame() {
        async fn reconnecting_upstream(
            req: HttpRequest,
            payload: web::Payload,
            attempts: web::Data<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            actix_web::rt::spawn(async move {
                if attempt == 0 {
                    drop(session);
                    return;
                }

                let mut stream = stream.aggregate_continuations();
                if let Some(Ok(AggregatedMessage::Text(text))) = stream.next().await {
                    let _ = session.text(text.to_string()).await;
                }
            });
            Ok(response)
        }

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let attempts_for_server = attempts.clone();
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(attempts_for_server.clone()))
                .route("/v1/responses", web::get().to(reconnecting_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let proxy_port = allocate_proxy_test_port();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal proxy starting");
            run_proxy_until_shutdown_blocking(
                ProxyConfig {
                    fence_url: upstream,
                    auth_method: AuthMethod::MasterKey("test-master".to_string()),
                    listen_port: proxy_port,
                    correlation_headers: Vec::new(),
                    local_api_key: None,
                    subscription_mode: false,
                    provider_auth_env_var: None,
                    protocol_diffs_dir: None,
                    verbose: false,
                    observe_request_duration: |_| {},
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .expect("proxy exits cleanly");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy thread should start");

        let mut last_err = None;
        let (mut ws, _) = 'connect: {
            for _ in 0..20 {
                match tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{proxy_port}/v1/responses"
                ))
                .await
                {
                    Ok(value) => break 'connect value,
                    Err(err) => {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            panic!("proxy did not accept websocket requests: {last_err:?}");
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        ws.send(TungsteniteMessage::Text("after-drop-reconnect".into()))
            .await
            .expect("client send should succeed after idle upstream drop");
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for echoed frame")
            .expect("websocket stream should yield a frame")
            .expect("websocket read should succeed");

        match msg {
            TungsteniteMessage::Text(text) => assert_eq!(text, "after-drop-reconnect"),
            other => panic!("expected echoed websocket text frame, got {other:?}"),
        }
        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "proxy should reconnect upstream after idle drop"
        );

        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_replies_to_client_websocket_close_frame() {
        async fn close_aware_upstream(
            req: HttpRequest,
            payload: web::Payload,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, session, mut stream) = actix_ws::handle(&req, payload)?;
            actix_web::rt::spawn(async move {
                if let Some(Ok(actix_ws::Message::Close(reason))) = stream.next().await {
                    let _ = session.close(reason).await;
                }
            });
            Ok(response)
        }

        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let upstream_server = HttpServer::new(|| {
            App::new().route("/v1/responses", web::get().to(close_aware_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let proxy_port = allocate_proxy_test_port();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal proxy starting");
            run_proxy_until_shutdown_blocking(
                ProxyConfig {
                    fence_url: upstream,
                    auth_method: AuthMethod::MasterKey("test-master".to_string()),
                    listen_port: proxy_port,
                    correlation_headers: Vec::new(),
                    local_api_key: None,
                    subscription_mode: false,
                    provider_auth_env_var: None,
                    protocol_diffs_dir: None,
                    verbose: false,
                    observe_request_duration: |_| {},
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .expect("proxy exits cleanly");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy thread should start");

        let mut last_err = None;
        let (mut ws, _) = 'connect: {
            for _ in 0..20 {
                match tokio_tungstenite::connect_async(format!(
                    "ws://127.0.0.1:{proxy_port}/v1/responses"
                ))
                .await
                {
                    Ok(value) => break 'connect value,
                    Err(err) => {
                        last_err = Some(err);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            panic!("proxy did not accept websocket requests: {last_err:?}");
        };

        ws.send(TungsteniteMessage::Close(Some(
            tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "client done".into(),
            },
        )))
        .await
        .expect("client close should be sent");

        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for proxy close reply")
            .expect("websocket stream should yield a close reply")
            .expect("websocket read should succeed");

        match msg {
            TungsteniteMessage::Close(Some(reason)) => {
                assert_eq!(u16::from(reason.code), 1000);
                assert_eq!(reason.reason, "client done");
            }
            other => panic!("expected proxy close reply, got {other:?}"),
        }

        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[test]
    fn proxy_server_starts_and_forwards_from_blocking_cli_runtime() {
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("local addr")
        );
        let upstream_thread = std::thread::spawn(move || {
            let (mut stream, _) = upstream_listener.accept().expect("accept upstream request");
            let mut buffer = [0_u8; 4096];
            let n = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("GET /v1/models HTTP/1.1"));
            assert!(request.contains("x-fence-auth: Bearer test-master"));
            assert!(request.contains("authorization: Bearer provider-token"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                )
                .expect("write response");
        });

        let proxy_port = allocate_proxy_test_port();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_thread = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal proxy starting");
            run_proxy_until_shutdown_blocking(
                ProxyConfig {
                    fence_url: upstream,
                    auth_method: AuthMethod::MasterKey("test-master".to_string()),
                    listen_port: proxy_port,
                    correlation_headers: Vec::new(),
                    local_api_key: None,
                    subscription_mode: false,
                    provider_auth_env_var: None,
                    protocol_diffs_dir: None,
                    verbose: false,
                    observe_request_duration: |_| {},
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .expect("proxy exits cleanly");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy thread should start");
        let client = reqwest::blocking::Client::new();
        let mut last_err = None;
        let response = (0..20)
            .find_map(|_| {
                match client
                    .get(format!("http://127.0.0.1:{proxy_port}/v1/models"))
                    .header("Authorization", "Bearer provider-token")
                    .send()
                {
                    Ok(response) => Some(response),
                    Err(err) => {
                        last_err = Some(err);
                        std::thread::sleep(Duration::from_millis(50));
                        None
                    }
                }
            })
            .unwrap_or_else(|| panic!("proxy did not accept requests: {last_err:?}"));

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_thread
            .join()
            .expect("upstream thread should complete");
    }

    #[test]
    fn client_env_commands_are_rendered_for_diagnostics_stream() {
        let mut output = Vec::new();
        write_client_env_commands(&mut output, 18181).expect("render env commands");
        let output = String::from_utf8(output).expect("utf8 output");

        assert!(output.contains("127.0.0.1:18181"));
        assert!(output.contains("OPENAI_BASE_URL"));
        assert!(output.contains("ANTHROPIC_BASE_URL"));
    }
}
