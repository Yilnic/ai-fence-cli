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
use futures::{FutureExt, SinkExt, StreamExt};
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
#[cfg(not(test))]
const LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(test)]
const LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(not(test))]
const LOCAL_PROXY_WEBSOCKET_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
#[cfg(test)]
const LOCAL_PROXY_WEBSOCKET_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const LOCAL_PROXY_WEBSOCKET_MAX_BACKOFF: Duration = Duration::from_secs(5);
#[cfg(test)]
const LOCAL_PROXY_WEBSOCKET_MAX_BACKOFF: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const LOCAL_PROXY_UPSTREAM_IDLE_PING_INTERVAL: Duration = Duration::from_secs(25);
#[cfg(test)]
const LOCAL_PROXY_UPSTREAM_IDLE_PING_INTERVAL: Duration = Duration::from_millis(50);

type LocalProxyUpstreamWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The local WebSocket has already been upgraded by the time an upstream
/// connection failure is reported. Keep enough structured information to send
/// a useful, non-sensitive error event to the client instead of flattening
/// every failure into a retry timeout.
#[derive(Debug)]
enum LocalProxyUpstreamWebSocketError {
    /// An upstream HTTP handshake response which cannot be fixed by waiting
    /// for a deployment to come back.
    PermanentHandshake { upstream_status: u16 },
    /// The fence rejected the OIDC access token and the local refresh token
    /// could not produce a replacement. This needs user action, not a
    /// two-minute network retry loop.
    AuthenticationRefresh { error: anyhow::Error },
    /// A connection failure that may be caused by an upstream deploy, network
    /// interruption, or temporary capacity problem.
    Unavailable {
        error: anyhow::Error,
        last_handshake_status: Option<u16>,
    },
}

impl LocalProxyUpstreamWebSocketError {
    fn from_tungstenite(error: tungstenite::Error) -> Self {
        match error {
            tungstenite::Error::Http(response)
                if is_permanent_upstream_websocket_handshake_status(response.status().as_u16()) =>
            {
                Self::PermanentHandshake {
                    upstream_status: response.status().as_u16(),
                }
            }
            tungstenite::Error::Http(response) => Self::Unavailable {
                last_handshake_status: Some(response.status().as_u16()),
                error: anyhow::Error::new(tungstenite::Error::Http(response)),
            },
            error => Self::Unavailable {
                error: anyhow::Error::new(error),
                last_handshake_status: None,
            },
        }
    }

    fn with_retry_timeout(self) -> Self {
        match self {
            Self::Unavailable {
                error,
                last_handshake_status,
            } => Self::Unavailable {
                error: error.context(format!(
                    "upstream WebSocket remained unavailable for {} seconds",
                    LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT.as_secs()
                )),
                last_handshake_status,
            },
            permanent => permanent,
        }
    }

    fn preserve_handshake_status(self, fallback_status: Option<u16>) -> Self {
        match self {
            Self::Unavailable {
                error,
                last_handshake_status,
            } => Self::Unavailable {
                error,
                last_handshake_status: last_handshake_status.or(fallback_status),
            },
            permanent => permanent,
        }
    }
}

impl std::fmt::Display for LocalProxyUpstreamWebSocketError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermanentHandshake { upstream_status } => write!(
                formatter,
                "upstream WebSocket handshake was rejected with HTTP {upstream_status}"
            ),
            Self::AuthenticationRefresh { error } => {
                write!(formatter, "OIDC credential refresh failed: {error:#}")
            }
            Self::Unavailable { error, .. } => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LocalProxyUpstreamWebSocketError {}

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
    /// OIDC JWT loaded from the shared credentials file for every request.
    ///
    /// Keeping the path rather than a token in proxy state lets another CLI
    /// process refresh the shared login without requiring every running proxy
    /// to be restarted.
    OidcTokenFile(PathBuf),
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
            if creds.is_expired() && creds.refresh_token.is_none() {
                anyhow::bail!(
                    "Stored OIDC token is expired and has no refresh token. Run `ai-fence-cli auth login` to refresh."
                );
            }
            Ok(AuthMethod::OidcTokenFile(crate::auth::credentials_path()?))
        } else if let Some(key) = master_key {
            Ok(AuthMethod::MasterKey(key))
        } else if let Some(creds) = crate::auth::load_credentials()? {
            if creds.is_expired() && creds.refresh_token.is_none() {
                anyhow::bail!(
                    "Stored OIDC token is expired and has no refresh token. Run `ai-fence-cli login` to refresh."
                );
            }
            Ok(AuthMethod::OidcTokenFile(crate::auth::credentials_path()?))
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
            AuthMethod::OidcTokenFile(path) => {
                let creds = crate::auth::load_credentials_from_path(path)?
                    .with_context(|| format!("No stored OIDC credentials at {}", path.display()))?;
                if creds.access_token.trim().is_empty() {
                    anyhow::bail!(
                        "Stored OIDC credentials at {} have an empty access token",
                        path.display()
                    );
                }
                Ok(vec![(
                    "x-fence-auth",
                    format!("Bearer {}", creds.access_token),
                )])
            }
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

    fn reloadable_oidc(&self) -> bool {
        matches!(self, Self::OidcTokenFile(_))
    }

    async fn ensure_fresh_oidc(&self) -> Result<()> {
        if let Self::OidcTokenFile(path) = self {
            crate::auth::ensure_fresh_credentials_from_path(path).await?;
        }
        Ok(())
    }

    async fn refresh_rejected_oidc(&self, rejected_access_token: &str) -> Result<()> {
        if let Self::OidcTokenFile(path) = self {
            crate::auth::refresh_credentials_after_rejection_from_path(path, rejected_access_token)
                .await?;
        }
        Ok(())
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
            AuthMethod::OidcTokenFile(_) => "OIDC JWT (file)",
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
    .workers(2)
    .shutdown_timeout(1)
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

    if let Err(error) = state.auth_method.ensure_fresh_oidc().await {
        warn!(error = %format!("{error:#}"), "Failed to refresh OIDC credentials before forwarding request");
        return Ok(oidc_refresh_failed_http_response());
    }

    let body = read_payload(payload).await?;
    let (upstream_body, synthesize_anthropic_json) =
        anthropic_backend_stream_body(&req, body.as_ref())?;

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
    let (upstream_request, sent_oidc_access_token) = build_upstream_http_request(
        &upstream_client,
        &upstream_method,
        &fence_url,
        &req,
        &state,
        &upstream_body,
    )?;
    let mut upstream_resp = upstream_request.send().await.map_err(|e| {
        warn!(error = %e, "Failed to forward request to fence");
        actix_web::error::ErrorInternalServerError(format!("Upstream request failed: {e}"))
    })?;

    // A refreshed OIDC token is written by another CLI process. If the
    // current request is rejected by the fence specifically for its auth
    // header, consume that response and retry exactly once with headers read
    // from the shared credentials file. Provider 401 responses are returned
    // unchanged, so an invalid provider credential cannot duplicate a request.
    if upstream_resp.status() == reqwest::StatusCode::UNAUTHORIZED
        && state.auth_method.reloadable_oidc()
    {
        let response_status = upstream_resp.status();
        let response_headers = upstream_resp.headers().clone();
        let response_body = upstream_resp
            .bytes()
            .await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
        if is_fence_auth_rejection(&response_body) {
            let Some(rejected_access_token) = sent_oidc_access_token.as_deref() else {
                return Ok(copy_buffered_upstream_response(
                    response_status,
                    &response_headers,
                    response_body,
                ));
            };
            tracing::info!(
                path = %req.uri().path(),
                "Fence rejected X-Fence-Auth; refreshing or reloading OIDC credentials and retrying once"
            );
            if let Err(error) = state
                .auth_method
                .refresh_rejected_oidc(rejected_access_token)
                .await
            {
                warn!(error = %format!("{error:#}"), "Failed to refresh rejected OIDC credentials");
                return Ok(oidc_refresh_failed_http_response());
            }
            let (retry_request, _) = build_upstream_http_request(
                &upstream_client,
                &upstream_method,
                &fence_url,
                &req,
                &state,
                &upstream_body,
            )?;
            upstream_resp = retry_request.send().await.map_err(|e| {
                warn!(error = %e, "Failed to retry request after reloading OIDC credentials");
                actix_web::error::ErrorInternalServerError(format!("Upstream request failed: {e}"))
            })?;
        } else {
            (state.observe_request_duration)(request_start.elapsed().as_secs_f64() * 1000.0);
            return Ok(copy_buffered_upstream_response(
                response_status,
                &response_headers,
                response_body,
            ));
        }
    }

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
        let (body, had_stream_error) = synthesize_anthropic_message_json(&stream_body)?;
        if had_stream_error {
            warn_reassembled_stream_error(&req, &body);
        }
        builder.insert_header(("content-type", "application/json"));
        Ok(builder.body(body))
    } else if content_type.contains("text/event-stream") || content_type.contains("text/plain") {
        // SSE streaming — forward as a streamed response body. The forwarded
        // bytes are never modified; a scanner shadows the chunks for terminal
        // error events so a failure riding inside a committed 200 response is
        // visible in the local CLI log (the client cannot back off from it,
        // and nothing else logs it on this leg).
        let path = req.uri().path().to_string();
        let mut scanner = SseErrorEventScanner::default();
        let mut warned = false;
        let stream = upstream_resp.bytes_stream().map(move |result| {
            result
                .inspect(|chunk| {
                    if !warned && scanner.scan(chunk) {
                        warned = true;
                        warn!(
                            path = %path,
                            "Upstream SSE stream carried a terminal error event inside a committed 200 response; client cannot back off"
                        );
                    }
                })
                .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))
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

fn build_upstream_http_request(
    upstream_client: &reqwest::Client,
    upstream_method: &reqwest::Method,
    fence_url: &str,
    req: &HttpRequest,
    state: &ProxyState,
    body: &[u8],
) -> Result<(reqwest::RequestBuilder, Option<String>), actix_web::Error> {
    let mut upstream_req = upstream_client
        .request(upstream_method.clone(), fence_url)
        .body(body.to_vec());
    let provider_auth_token = state.provider_auth_token();

    // Copy relevant headers from the client request.
    for (name, value) in req.headers() {
        let name_lower = name.as_str().to_lowercase();
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

    let auth_headers = state.auth_method.headers().map_err(|e| {
        warn!(error = %e, "Failed to resolve proxy authentication headers");
        actix_web::error::ErrorInternalServerError(format!(
            "Failed to resolve proxy authentication headers: {e}"
        ))
    })?;
    let mut oidc_access_token = None;
    for (name, value) in auth_headers {
        if name == "x-fence-auth" && state.auth_method.reloadable_oidc() {
            oidc_access_token = value.strip_prefix("Bearer ").map(ToOwned::to_owned);
        }
        upstream_req = upstream_req.header(name, value);
    }
    for (name, value) in &state.correlation_headers {
        upstream_req = upstream_req.header(name.as_str(), value.as_str());
    }
    upstream_req = upstream_req
        .header("accept-encoding", "identity")
        .header("x-fence-local-proxy", "true")
        .header("x-fence-stream-keepalive", "true");

    if let Some(ref dir) = state.protocol_diffs_dir {
        upstream_req =
            upstream_req.header("x-fence-protocol-diffs-dir", dir.to_string_lossy().as_ref());
    }

    Ok((upstream_req, oidc_access_token))
}

fn copy_buffered_upstream_response(
    response_status: reqwest::StatusCode,
    response_headers: &reqwest::header::HeaderMap,
    response_body: bytes::Bytes,
) -> HttpResponse {
    let status = ActixStatus::from_u16(response_status.as_u16())
        .unwrap_or(ActixStatus::INTERNAL_SERVER_ERROR);
    let mut builder = HttpResponse::build(status);
    for (name, value) in response_headers {
        let name_lower = name.as_str().to_lowercase();
        if matches!(
            name_lower.as_str(),
            "transfer-encoding" | "connection" | "keep-alive"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            builder.insert_header((name.as_str(), value));
        }
    }
    builder.body(response_body)
}

fn oidc_refresh_failed_http_response() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({
        "error": {
            "type": "authentication_error",
            "code": "oidc_refresh_failed",
            "message": "The stored AI Fence login could not be refreshed automatically. Run `ai-fence-cli login` once, then retry."
        }
    }))
}

fn is_fence_auth_rejection(body: &[u8]) -> bool {
    let body = String::from_utf8_lossy(body).to_ascii_lowercase();
    body.contains("x-fence-auth") || body.contains("fence auth")
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

/// Log a reassembled in-stream upstream error. The body is an error envelope
/// (already sanitized by the backend) returned with a committed 200 status —
/// the client sees "not a Message" instead of a retryable status, so this
/// warn is the only local trace of the conversion.
fn warn_reassembled_stream_error(req: &HttpRequest, body: &[u8]) {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let error = value.get("error");
    warn!(
        path = %req.uri().path(),
        status = error
            .and_then(|e| e.get("status"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
        error_type = error
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("unknown"),
        message = error
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or(""),
        "Reassembled upstream stream error into a 200 non-Message body; client cannot back off"
    );
}

/// Terminal upstream SSE error frame prefix as emitted by the fence
/// (`event: error\ndata: {"type":"error"...`). Matched as one marker so prose
/// that merely discusses SSE errors cannot trip the scanner.
const SSE_ERROR_FRAME_MARKER: &[u8] = b"event: error\ndata: {\"type\":\"error\"";

/// Scans forwarded SSE chunks for terminal error frames across chunk
/// boundaries without modifying the forwarded bytes. A carry buffer closes
/// gaps when the marker splits across chunks.
#[derive(Default)]
struct SseErrorEventScanner {
    carry: Vec<u8>,
}

impl SseErrorEventScanner {
    fn scan(&mut self, chunk: &[u8]) -> bool {
        let mut buf = Vec::with_capacity(self.carry.len() + chunk.len());
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(chunk);
        let found = buf
            .windows(SSE_ERROR_FRAME_MARKER.len())
            .any(|w| w == SSE_ERROR_FRAME_MARKER);
        // Retain only a tail long enough to close a marker split across the
        // next chunk boundary.
        let keep = SSE_ERROR_FRAME_MARKER
            .len()
            .saturating_sub(1)
            .min(buf.len());
        self.carry = buf[buf.len() - keep..].to_vec();
        found
    }
}

/// Reassemble a forced-streaming Anthropic response back into a single
/// non-streaming Message JSON. Returns the body plus whether the stream
/// carried a terminal `error` event instead of a message: that body is an
/// error envelope riding inside an already-committed 200 response — the
/// least discoverable failure mode in the stack, which callers must log.
fn synthesize_anthropic_message_json(
    stream_body: &[u8],
) -> Result<(Vec<u8>, bool), actix_web::Error> {
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
            .map(|body| (body, true))
            .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()));
    }

    for block in &mut content {
        finalize_aggregated_tool_use(block);
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
        .map(|body| (body, false))
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

/// A non-streaming Anthropic message carries parsed tool arguments in
/// `input`; the aggregated upstream stream only accumulates the raw
/// `partial_json` fragments. Parse them once aggregation completes and
/// drop the streaming-only field, so spec-conforming clients that read
/// `input` (e.g. Junie's harness) see the arguments instead of an empty
/// object.
fn finalize_aggregated_tool_use(block: &mut serde_json::Value) {
    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
        return;
    }
    let Some(map) = block.as_object_mut() else {
        return;
    };
    let partial = map.remove("partial_json");
    let has_parsed_input = map
        .get("input")
        .and_then(|value| value.as_object())
        .map(|input| !input.is_empty())
        .unwrap_or(false);
    if has_parsed_input {
        return;
    }
    let input = partial
        .as_ref()
        .and_then(|value| value.as_str())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    map.insert("input".to_string(), input);
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
    state.auth_method.ensure_fresh_oidc().await.map_err(|error| {
        warn!(error = %format!("{error:#}"), "Failed to refresh OIDC credentials before WebSocket connection");
        actix_web::error::ErrorUnauthorized(
            "Stored AI Fence login could not be refreshed automatically. Run `ai-fence-cli login` once, then retry.",
        )
    })?;
    let (mut response, mut client_session, client_stream) = actix_ws::handle(&req, payload)?;
    add_codex_websocket_headers(&mut response);
    let mut client_stream = client_stream
        .max_frame_size(LOCAL_PROXY_MAX_WEBSOCKET_MESSAGE_BYTES)
        .aggregate_continuations()
        .max_continuation_size(LOCAL_PROXY_MAX_WEBSOCKET_MESSAGE_BYTES);

    let upstream_request = build_upstream_websocket_request(&req, &state, &fence_url)?;
    let request_context = req.clone();
    actix_web::rt::spawn(async move {
        // Do not make the local WebSocket's lifetime depend on a single
        // upstream connection attempt. In particular, Codex keeps this local
        // connection around for subsequent turns. If the fence is temporarily
        // unavailable, the failed turn must not make every later turn require
        // a launcher/Codex restart.
        //
        // Codex sends its first request while the initial upstream handshake
        // is in progress. If that bounded handshake fails, discard that one
        // buffered request after reporting its terminal error; it is never
        // replayed later when the fence returns.
        let (mut upstream_sink, mut upstream_stream, mut discard_initial_failed_request): (
            Option<futures::stream::SplitSink<LocalProxyUpstreamWebSocket, tungstenite::Message>>,
            Option<futures::stream::SplitStream<LocalProxyUpstreamWebSocket>>,
            bool,
        ) = match connect_upstream_websocket_with_auth_reload(
            upstream_request,
            &request_context,
            &state,
            &fence_url,
            state.observe_request_duration,
        )
        .await
        {
            Ok(ws) => {
                let (sink, stream) = ws.split();
                (Some(sink), Some(stream), false)
            }
            Err(err) => {
                warn!(error = %err, "Failed to connect upstream WebSocket; failing initial request without replay");
                send_local_proxy_websocket_error(&mut client_session, &err).await;
                (None, None, true)
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

                    let is_request_payload = matches!(
                        sent_message,
                        tungstenite::Message::Text(_) | tungstenite::Message::Binary(_)
                    );

                    if discard_initial_failed_request && is_request_payload {
                        discard_initial_failed_request = false;
                        tracing::info!(
                            "Discarding request buffered during failed initial upstream WebSocket handshake"
                        );
                        continue;
                    }

                    // Client ping/pong frames must not begin a 120-second
                    // reconnect attempt while the upstream is down. They are
                    // transport maintenance, not a request to replay.
                    if upstream_sink.is_none() && !is_request_payload {
                        continue;
                    }

                    // Give an already-buffered idle upstream close/error a
                    // chance to win over this new request. The old upstream
                    // has not seen this frame yet, so reconnecting here is
                    // safe and preserves the normal idle-reconnect path.
                    // Once `send` below is attempted, this proxy deliberately
                    // never retries that frame because delivery is ambiguous.
                    if is_request_payload && !upstream_in_flight {
                        let queued_upstream_message = upstream_stream
                            .as_mut()
                            .and_then(|stream| stream.next().now_or_never());
                        match queued_upstream_message {
                            Some(None) => {
                                tracing::debug!(
                                    "Idle upstream WebSocket stream ended before forwarding client frame"
                                );
                                upstream_sink = None;
                                upstream_stream = None;
                            }
                            Some(Some(Err(err))) => {
                                if is_tungstenite_websocket_eof_error(&err) {
                                    tracing::debug!(
                                        error = %err,
                                        "Idle upstream WebSocket disconnected before forwarding client frame"
                                    );
                                } else {
                                    warn!(
                                        error = %err,
                                        "Idle upstream WebSocket read failed before forwarding client frame"
                                    );
                                }
                                upstream_sink = None;
                                upstream_stream = None;
                            }
                            Some(Some(Ok(tungstenite::Message::Close(reason)))) => {
                                tracing::debug!(
                                    ?reason,
                                    "Idle upstream WebSocket closed before forwarding client frame"
                                );
                                upstream_sink = None;
                                upstream_stream = None;
                            }
                            Some(Some(Ok(message))) => {
                                if !forward_upstream_ws_message(message, &mut client_session).await {
                                    break;
                                }
                            }
                            None => {}
                        }
                    }

                    if upstream_sink.is_none() {
                        tracing::info!("Reconnecting local proxy upstream WebSocket before forwarding client frame");
                        match reconnect_upstream_websocket_with_auth_reload(
                            &request_context,
                            &state,
                            &fence_url,
                            state.observe_request_duration,
                        )
                        .await {
                            Ok(ws) => {
                                let (sink, stream) = ws.split();
                                upstream_sink = Some(sink);
                                upstream_stream = Some(stream);
                            }
                            Err(err) => {
                                warn!(error = %err, "Failed to reconnect upstream WebSocket; failing affected request without replay");
                                send_local_proxy_websocket_error(&mut client_session, &err).await;
                                // Keep the local WebSocket open. A later
                                // request gets a fresh bounded reconnect
                                // attempt and can succeed after the outage.
                                continue;
                            }
                        }
                    }

                    let send_result = upstream_sink
                        .as_mut()
                        .expect("checked upstream connection")
                        .send(sent_message)
                        .await;
                    if let Err(err) = send_result {
                        warn!(error = %err, "Local proxy upstream WebSocket send failed; failing affected request without replay");
                        upstream_sink = None;
                        upstream_stream = None;
                        if is_request_payload {
                            let error = LocalProxyUpstreamWebSocketError::Unavailable {
                                error: anyhow::Error::new(err).context(
                                    "upstream WebSocket send failed before the response completed",
                                ),
                                last_handshake_status: None,
                            };
                            send_local_proxy_websocket_error(&mut client_session, &error).await;
                        }
                        // A following client request will establish a new
                        // upstream connection. Do not resend this frame: the
                        // original peer might have received it before the
                        // send operation reported a transport failure.
                        upstream_in_flight = false;
                        continue;
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
                            warn!("Upstream WebSocket closed before the response completed; failing affected request without replay");
                            send_local_proxy_websocket_error(
                                &mut client_session,
                                &upstream_websocket_interrupted_error(
                                    "upstream WebSocket closed before the response completed",
                                ),
                            )
                            .await;
                            upstream_in_flight = false;
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
                                send_local_proxy_websocket_error(
                                    &mut client_session,
                                    &upstream_websocket_interrupted_error(format!(
                                        "upstream WebSocket read failed before the response completed: {err}"
                                    )),
                                )
                                .await;
                                upstream_in_flight = false;
                            }
                            continue;
                        }
                    };
                    let upstream_finished = local_proxy_upstream_message_finishes(&upstream_msg);
                    match upstream_msg {
                        tungstenite::Message::Close(reason) => {
                            upstream_sink = None;
                            upstream_stream = None;
                            if upstream_in_flight {
                                warn!(?reason, "Upstream WebSocket closed before the response completed; failing affected request without replay");
                                send_local_proxy_websocket_error(
                                    &mut client_session,
                                    &upstream_websocket_interrupted_error(
                                        "upstream WebSocket closed before the response completed",
                                    ),
                                )
                                .await;
                                upstream_in_flight = false;
                            } else {
                                tracing::debug!(?reason, "Idle upstream WebSocket closed; keeping local WebSocket open");
                            }
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
) -> std::result::Result<LocalProxyUpstreamWebSocket, LocalProxyUpstreamWebSocketError> {
    let connect_start = Instant::now();
    let retry_deadline = connect_start + LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT;
    let mut attempt = 0_u32;
    let mut backoff = LOCAL_PROXY_WEBSOCKET_INITIAL_BACKOFF;
    let mut last_error = None;
    let mut last_handshake_status = None;

    loop {
        attempt += 1;
        let remaining = retry_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LocalProxyUpstreamWebSocketError::Unavailable {
                error: last_error.unwrap_or_else(|| {
                    anyhow::anyhow!("upstream WebSocket connection retry deadline elapsed")
                }),
                last_handshake_status,
            }
            .with_retry_timeout());
        }

        match tokio::time::timeout(remaining, connect_async(request.clone())).await {
            Ok(Ok((ws, _))) => {
                observe_request_duration(connect_start.elapsed().as_secs_f64() * 1000.0);
                if attempt > 1 {
                    tracing::info!(
                        attempt,
                        unavailable_ms = connect_start.elapsed().as_millis(),
                        "Local proxy upstream WebSocket reconnected"
                    );
                }
                return Ok(ws);
            }
            Ok(Err(err)) => {
                let error = LocalProxyUpstreamWebSocketError::from_tungstenite(err);
                if matches!(
                    &error,
                    LocalProxyUpstreamWebSocketError::PermanentHandshake { .. }
                ) {
                    return Err(error);
                }
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error
                        .preserve_handshake_status(last_handshake_status)
                        .with_retry_timeout());
                }
                let retry_after = std::cmp::min(backoff, remaining);
                if attempt == 1 {
                    warn!(
                        attempt,
                        retry_after_ms = retry_after.as_millis(),
                        retry_timeout_secs = LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT.as_secs(),
                        error = %error,
                        "Local proxy upstream WebSocket unavailable; retrying"
                    );
                } else {
                    tracing::debug!(
                        attempt,
                        retry_after_ms = retry_after.as_millis(),
                        error = %error,
                        "Local proxy upstream WebSocket still unavailable"
                    );
                }
                let LocalProxyUpstreamWebSocketError::Unavailable {
                    error,
                    last_handshake_status: handshake_status,
                } = error
                else {
                    unreachable!("permanent handshake errors return before retrying");
                };
                if handshake_status.is_some() {
                    last_handshake_status = handshake_status;
                }
                last_error = Some(error);
                sleep(retry_after).await;
                backoff =
                    std::cmp::min(backoff.saturating_mul(2), LOCAL_PROXY_WEBSOCKET_MAX_BACKOFF);
            }
            Err(_) => {
                return Err(LocalProxyUpstreamWebSocketError::Unavailable {
                    error: anyhow::anyhow!(
                        "upstream WebSocket connection timed out after {} seconds",
                        LOCAL_PROXY_WEBSOCKET_RETRY_TIMEOUT.as_secs()
                    ),
                    last_handshake_status,
                });
            }
        }
    }
}

async fn connect_upstream_websocket_with_auth_reload(
    request: tungstenite::handshake::client::Request,
    request_context: &HttpRequest,
    state: &ProxyState,
    fence_url: &str,
    observe_request_duration: fn(f64),
) -> std::result::Result<LocalProxyUpstreamWebSocket, LocalProxyUpstreamWebSocketError> {
    let sent_oidc_access_token = websocket_oidc_access_token(&request);
    match connect_local_proxy_upstream_websocket(request, observe_request_duration).await {
        Err(LocalProxyUpstreamWebSocketError::PermanentHandshake {
            upstream_status: 401,
        }) if state.auth_method.reloadable_oidc() => {
            tracing::info!(
                "Fence rejected WebSocket X-Fence-Auth; refreshing or reloading OIDC credentials and retrying once"
            );
            let Some(rejected_access_token) = sent_oidc_access_token.as_deref() else {
                return Err(LocalProxyUpstreamWebSocketError::PermanentHandshake {
                    upstream_status: 401,
                });
            };
            state
                .auth_method
                .refresh_rejected_oidc(rejected_access_token)
                .await
                .map_err(
                    |error| LocalProxyUpstreamWebSocketError::AuthenticationRefresh { error },
                )?;
            let refreshed_request =
                build_upstream_websocket_request(request_context, state, fence_url)
                    .map_err(websocket_request_build_error)?;
            connect_local_proxy_upstream_websocket(refreshed_request, observe_request_duration)
                .await
        }
        result => result,
    }
}

async fn reconnect_upstream_websocket_with_auth_reload(
    request_context: &HttpRequest,
    state: &ProxyState,
    fence_url: &str,
    observe_request_duration: fn(f64),
) -> std::result::Result<LocalProxyUpstreamWebSocket, LocalProxyUpstreamWebSocketError> {
    state
        .auth_method
        .ensure_fresh_oidc()
        .await
        .map_err(|error| LocalProxyUpstreamWebSocketError::AuthenticationRefresh { error })?;
    let request = build_upstream_websocket_request(request_context, state, fence_url)
        .map_err(websocket_request_build_error)?;
    connect_upstream_websocket_with_auth_reload(
        request,
        request_context,
        state,
        fence_url,
        observe_request_duration,
    )
    .await
}

fn websocket_oidc_access_token(
    request: &tungstenite::handshake::client::Request,
) -> Option<String> {
    request
        .headers()
        .get("x-fence-auth")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(ToOwned::to_owned)
}

fn websocket_request_build_error(error: actix_web::Error) -> LocalProxyUpstreamWebSocketError {
    LocalProxyUpstreamWebSocketError::Unavailable {
        error: anyhow::anyhow!(error.to_string()),
        last_handshake_status: None,
    }
}

/// Report a failed client turn while deliberately preserving the local
/// WebSocket. A Codex process can use the same socket for later turns after a
/// temporary fence outage; closing it would make a one-turn failure terminal
/// for that process.
async fn send_local_proxy_websocket_error(
    client_session: &mut actix_ws::Session,
    err: &LocalProxyUpstreamWebSocketError,
) {
    let _ = client_session
        .text(websocket_connect_error_payload(err).to_string())
        .await;
}

fn upstream_websocket_interrupted_error(
    message: impl std::fmt::Display,
) -> LocalProxyUpstreamWebSocketError {
    LocalProxyUpstreamWebSocketError::Unavailable {
        error: anyhow::anyhow!("{message}"),
        last_handshake_status: None,
    }
}

fn is_permanent_upstream_websocket_handshake_status(status: u16) -> bool {
    // 408 and 429 can recover without a client configuration change. Do not
    // treat opaque proxy/gateway statuses (for example Cloudflare's 520) as
    // permanent: the original backend 401 may no longer be visible there.
    matches!(
        status,
        400 | 401 | 403 | 404 | 405 | 406 | 407 | 410 | 411 | 413 | 414 | 415 | 422
    )
}

fn websocket_connect_error_payload(error: &LocalProxyUpstreamWebSocketError) -> serde_json::Value {
    match error {
        LocalProxyUpstreamWebSocketError::AuthenticationRefresh { .. } => serde_json::json!({
            "type": "error",
            "status": 401,
            "error": {
                "type": "authentication_error",
                "message": "The stored AI Fence login could not be refreshed automatically. Run `ai-fence-cli login` once, then retry.",
                "code": "oidc_refresh_failed"
            }
        }),
        LocalProxyUpstreamWebSocketError::PermanentHandshake {
            upstream_status: 401,
        } => serde_json::json!({
            "type": "error",
            "status": 401,
            "error": {
                "type": "authentication_error",
                "message": "Upstream WebSocket authentication was rejected. Restart Codex with a valid provider login or configured API key.",
                "code": "upstream_authentication_failed"
            }
        }),
        LocalProxyUpstreamWebSocketError::PermanentHandshake {
            upstream_status: 403,
        } => serde_json::json!({
            "type": "error",
            "status": 403,
            "error": {
                "type": "permission_error",
                "message": "Upstream WebSocket authorization was denied. Check the account and project permissions used by this session.",
                "code": "upstream_authorization_failed"
            }
        }),
        LocalProxyUpstreamWebSocketError::PermanentHandshake { upstream_status } => {
            serde_json::json!({
                "type": "error",
                "status": upstream_status,
                "error": {
                    "type": "invalid_request_error",
                    "message": format!("Upstream WebSocket handshake was rejected with HTTP {upstream_status}. Check the local proxy and client configuration."),
                    "code": "upstream_handshake_rejected"
                }
            })
        }
        LocalProxyUpstreamWebSocketError::Unavailable {
            error,
            last_handshake_status: Some(520),
        } => serde_json::json!({
            "type": "error",
            "status": 502,
            "error": {
                "type": "server_error",
                "message": format!("Upstream WebSocket connection failed: {error}. The upstream gateway returned HTTP 520, which can mask rejected provider authentication. Verify provider credentials are being forwarded, then retry."),
                "code": "upstream_gateway_error",
                "upstream_status": 520
            }
        }),
        LocalProxyUpstreamWebSocketError::Unavailable {
            error,
            last_handshake_status,
        } => {
            let mut payload = serde_json::json!({
                "type": "error",
                "status": 502,
                "error": {
                    "type": "server_error",
                    "message": format!("Upstream WebSocket connection failed: {error}"),
                    "code": "server_error"
                }
            });
            if let Some(upstream_status) = last_handshake_status {
                payload["error"]["upstream_status"] = serde_json::json!(upstream_status);
            }
            payload
        }
    }
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
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    static NEXT_PROXY_TEST_PORT: AtomicU16 = AtomicU16::new(32000);
    static AUTH_FILE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestCredentialsScope {
        _lock: MutexGuard<'static, ()>,
        previous_channel: Option<OsString>,
        directory: std::path::PathBuf,
    }

    impl TestCredentialsScope {
        fn new() -> Self {
            let lock = AUTH_FILE_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_channel = std::env::var_os("AI_FENCE_CLI_CHANNEL");
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("test clock should be after unix epoch")
                .as_nanos();
            let channel = format!("proxy-auth-test-{}-{nonce}", std::process::id());
            let directory = crate::config::config_dir_for_channel(Some(&channel))
                .expect("create isolated test config directory");
            std::env::set_var("AI_FENCE_CLI_CHANNEL", &channel);
            Self {
                _lock: lock,
                previous_channel,
                directory,
            }
        }

        fn write_credentials(&self, access_token: &str) {
            write_test_credentials(access_token);
        }

        fn write_credentials_for_issuer(
            &self,
            access_token: &str,
            refresh_token: &str,
            issuer: &str,
            expires_at: i64,
        ) {
            crate::auth::save_credentials(&crate::auth::StoredCredentials {
                access_token: access_token.to_string(),
                refresh_token: Some(refresh_token.to_string()),
                expires_at: Some(expires_at),
                issuer: issuer.to_string(),
                client_id: "ai-fence-cli-test".to_string(),
            })
            .expect("write test credentials");
        }
    }

    impl Drop for TestCredentialsScope {
        fn drop(&mut self) {
            match self.previous_channel.take() {
                Some(value) => std::env::set_var("AI_FENCE_CLI_CHANNEL", value),
                None => std::env::remove_var("AI_FENCE_CLI_CHANNEL"),
            }
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn write_test_credentials(access_token: &str) {
        crate::auth::save_credentials(&crate::auth::StoredCredentials {
            access_token: access_token.to_string(),
            refresh_token: Some("refresh-test-token".to_string()),
            expires_at: Some(chrono::Utc::now().timestamp() + 3600),
            issuer: "https://id.example.test/".to_string(),
            client_id: "ai-fence-cli-test".to_string(),
        })
        .expect("write test credentials");
    }

    fn forwarded_fence_auth(request: &str) -> String {
        request
            .lines()
            .find_map(|line| line.strip_prefix("x-fence-auth: "))
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn oidc_proxy_headers_follow_shared_credential_file_updates() {
        let scope = TestCredentialsScope::new();
        scope.write_credentials("old-access-token");
        let auth = AuthMethod::resolve(None, true, None, None, None)
            .expect("initial OIDC credentials should resolve");
        assert_eq!(
            auth.headers().expect("initial auth headers"),
            vec![("x-fence-auth", "Bearer old-access-token".to_string())]
        );

        scope.write_credentials("new-access-token");

        assert_eq!(
            auth.headers()
                .expect("auth headers should reload the shared credentials file"),
            vec![("x-fence-auth", "Bearer new-access-token".to_string())]
        );
    }

    #[test]
    fn expired_oidc_with_refresh_token_can_start_the_proxy() {
        let scope = TestCredentialsScope::new();
        scope.write_credentials_for_issuer(
            "expired-access-token",
            "usable-refresh-token",
            "https://id.example.test/",
            chrono::Utc::now().timestamp() - 60,
        );

        let auth = AuthMethod::resolve(None, true, None, None, None)
            .expect("runtime refresh should be allowed to recover the proxy");

        assert!(matches!(auth, AuthMethod::OidcTokenFile(_)));
    }

    #[test]
    fn saved_oidc_credentials_are_private() {
        let scope = TestCredentialsScope::new();
        scope.write_credentials("private-access-token");
        let path = crate::auth::credentials_path().expect("credentials path");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .expect("credentials metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "OIDC credentials must not be world-readable");
        }
    }

    #[cfg(unix)]
    #[test]
    fn saving_oidc_credentials_replaces_the_file_atomically() {
        let scope = TestCredentialsScope::new();
        scope.write_credentials("old-access-token");
        let path = crate::auth::credentials_path().expect("credentials path");
        let mut old_file = std::fs::File::open(&path).expect("open old credentials file");

        scope.write_credentials("new-access-token");

        let mut old_snapshot = String::new();
        old_file
            .read_to_string(&mut old_snapshot)
            .expect("read old credentials snapshot");
        let old_credentials: crate::auth::StoredCredentials =
            serde_json::from_str(&old_snapshot).expect("old snapshot remains valid JSON");
        assert_eq!(old_credentials.access_token, "old-access-token");
        assert_eq!(
            crate::auth::load_credentials()
                .expect("load current credentials")
                .expect("current credentials")
                .access_token,
            "new-access-token"
        );
    }

    #[actix_web::test]
    async fn proxy_retries_a_401_with_rotated_oidc_credentials() {
        let scope = TestCredentialsScope::new();
        scope.write_credentials("old-access-token");
        let auth_method = AuthMethod::resolve(None, true, None, None, None)
            .expect("initial OIDC credentials should resolve");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        listener
            .set_nonblocking(true)
            .expect("make mock upstream nonblocking");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut observed = Vec::new();
            while observed.len() < 2 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept upstream request: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set mock read timeout");
                let mut buffer = [0_u8; 4096];
                let n = stream.read(&mut buffer).expect("read upstream request");
                let request = String::from_utf8_lossy(&buffer[..n]);
                observed.push(forwarded_fence_auth(&request));
                if observed.len() == 1 {
                    write_test_credentials("new-access-token");
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nconnection: close\r\ncontent-length: 26\r\n\r\nInvalid X-Fence-Auth token",
                        )
                        .expect("write auth rejection");
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
                        )
                        .expect("write successful retry");
                }
            }
            observed
        });

        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method,
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
            .set_payload("{}")
            .to_request();
        let response = actix_web::test::call_service(&app, req).await;
        assert_eq!(response.status(), ActixStatus::OK);

        let observed = server.join().expect("mock upstream should complete");
        assert_eq!(
            observed,
            vec![
                "Bearer old-access-token".to_string(),
                "Bearer new-access-token".to_string()
            ]
        );
    }

    #[actix_web::test]
    async fn proxy_refreshes_rejected_oidc_credentials_and_retries_the_request() {
        let scope = TestCredentialsScope::new();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock fence and issuer");
        listener
            .set_nonblocking(true)
            .expect("make mock server nonblocking");
        let upstream = format!("http://{}", listener.local_addr().expect("local addr"));
        scope.write_credentials_for_issuer(
            "rejected-access-token",
            "rotating-refresh-token",
            &upstream,
            chrono::Utc::now().timestamp() + 3600,
        );
        let token_endpoint = format!("{upstream}/token");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut observed_auth = Vec::new();
            let mut refresh_calls = 0;
            while Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept mock request: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set mock read timeout");
                let mut buffer = [0_u8; 8192];
                let bytes_read = stream.read(&mut buffer).expect("read mock request");
                let request = String::from_utf8_lossy(&buffer[..bytes_read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default();
                let (status, body) = match path {
                    "/.well-known/openid-configuration" => (
                        "200 OK",
                        serde_json::json!({
                            "device_authorization_endpoint": format!("{token_endpoint}/device"),
                            "token_endpoint": token_endpoint,
                        })
                        .to_string(),
                    ),
                    "/token" => {
                        refresh_calls += 1;
                        (
                            "200 OK",
                            serde_json::json!({
                                "access_token": "refreshed-access-token",
                                "refresh_token": "rotated-refresh-token",
                                "expires_in": 3600,
                                "token_type": "Bearer",
                            })
                            .to_string(),
                        )
                    }
                    "/v1/responses" => {
                        let auth = forwarded_fence_auth(&request);
                        observed_auth.push(auth.clone());
                        if auth == "Bearer refreshed-access-token" {
                            let body = serde_json::json!({"ok": true}).to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            stream
                                .write_all(response.as_bytes())
                                .expect("write successful response");
                            return (observed_auth, refresh_calls);
                        }
                        ("401 Unauthorized", "Invalid X-Fence-Auth token".to_string())
                    }
                    _ => panic!("unexpected mock path: {path}"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write mock response");
            }
            (observed_auth, refresh_calls)
        });

        let state = web::Data::new(ProxyState {
            fence_url: upstream,
            auth_method: AuthMethod::OidcTokenFile(
                crate::auth::credentials_path().expect("credential path"),
            ),
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
            .set_payload("{}")
            .to_request();
        let response = actix_web::test::call_service(&app, req).await;
        assert_eq!(response.status(), ActixStatus::OK);

        let (observed_auth, refresh_calls) = server.join().expect("mock server should complete");
        assert_eq!(refresh_calls, 1);
        assert_eq!(
            observed_auth,
            vec![
                "Bearer rejected-access-token".to_string(),
                "Bearer refreshed-access-token".to_string(),
            ]
        );
    }

    #[actix_web::test]
    async fn websocket_handshake_refreshes_rejected_oidc_credentials() {
        #[derive(Clone)]
        struct RefreshServerState {
            token_endpoint: String,
            refresh_calls: Arc<AtomicUsize>,
        }

        async fn oidc_discovery(
            state: web::Data<RefreshServerState>,
        ) -> web::Json<serde_json::Value> {
            web::Json(serde_json::json!({
                "device_authorization_endpoint": format!("{}/device", state.token_endpoint),
                "token_endpoint": state.token_endpoint,
            }))
        }

        async fn oidc_refresh(
            state: web::Data<RefreshServerState>,
        ) -> web::Json<serde_json::Value> {
            state.refresh_calls.fetch_add(1, Ordering::SeqCst);
            web::Json(serde_json::json!({
                "access_token": "new-access-token",
                "refresh_token": "new-refresh-token",
                "expires_in": 3600,
                "token_type": "Bearer",
            }))
        }

        async fn refreshing_upstream(
            req: HttpRequest,
            payload: web::Payload,
        ) -> Result<HttpResponse, actix_web::Error> {
            let auth = req
                .headers()
                .get("x-fence-auth")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if auth == "Bearer old-access-token" {
                return Ok(HttpResponse::Unauthorized().finish());
            }
            assert_eq!(auth, "Bearer new-access-token");
            let (response, _session, _stream) = actix_ws::handle(&req, payload)?;
            Ok(response)
        }

        let scope = TestCredentialsScope::new();
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let upstream_url = format!("{upstream}/v1/responses");
        scope.write_credentials_for_issuer(
            "old-access-token",
            "old-refresh-token",
            &upstream,
            chrono::Utc::now().timestamp() + 3600,
        );
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let refresh_state = RefreshServerState {
            token_endpoint: format!("{upstream}/token"),
            refresh_calls: Arc::clone(&refresh_calls),
        };
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(refresh_state.clone()))
                .route(
                    "/.well-known/openid-configuration",
                    web::get().to(oidc_discovery),
                )
                .route("/token", web::post().to(oidc_refresh))
                .route("/v1/responses", web::get().to(refreshing_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let state = ProxyState {
            fence_url: upstream.clone(),
            auth_method: AuthMethod::resolve(None, true, None, None, None)
                .expect("resolve file-backed OIDC auth"),
            correlation_headers: Vec::new(),
            local_api_key: None,
            subscription_mode: false,
            provider_auth_env_var: None,
            protocol_diffs_dir: None,
            observe_request_duration: |_| {},
        };
        let request_context = actix_web::test::TestRequest::get()
            .uri("/v1/responses")
            .to_http_request();
        let request = build_upstream_websocket_request(&request_context, &state, &upstream_url)
            .expect("build upstream WebSocket request");
        let websocket = tokio::time::timeout(
            Duration::from_secs(1),
            connect_upstream_websocket_with_auth_reload(
                request,
                &request_context,
                &state,
                &upstream_url,
                |_| {},
            ),
        )
        .await
        .expect("rotated OIDC handshake should complete promptly")
        .expect("rotated OIDC handshake should succeed");
        drop(websocket);

        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
        let refreshed = crate::auth::load_credentials()
            .expect("load refreshed credentials")
            .expect("refreshed credentials");
        assert_eq!(refreshed.access_token, "new-access-token");
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("new-refresh-token")
        );

        upstream_handle.stop(true).await;
    }

    #[test]
    fn proxy_auth_retry_detection_does_not_match_provider_errors() {
        assert!(is_fence_auth_rejection(b"Invalid X-Fence-Auth token"));
        assert!(is_fence_auth_rejection(
            b"authentication required (X-Fence-Auth header)"
        ));
        assert!(!is_fence_auth_rejection(b"Invalid provider token"));
        assert!(!is_fence_auth_rejection(b"Upstream provider error"));
    }

    #[test]
    fn websocket_oidc_refresh_failure_is_safe_and_actionable() {
        let payload = websocket_connect_error_payload(
            &LocalProxyUpstreamWebSocketError::AuthenticationRefresh {
                error: anyhow::anyhow!("provider detail with secret-refresh-token"),
            },
        );

        assert_eq!(payload["status"], 401);
        assert_eq!(payload["error"]["code"], "oidc_refresh_failed");
        assert!(payload["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("ai-fence-cli login")));
        assert!(!payload.to_string().contains("secret-refresh-token"));
    }

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

    #[test]
    fn synthesized_non_streaming_message_parses_split_tool_use_input() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tools\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"glm-5.3\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"run_bash\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"echo hello\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":6}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (body, had_stream_error) =
            synthesize_anthropic_message_json(stream.as_bytes()).expect("synthesize");
        assert!(!had_stream_error);
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let block = &value["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["name"], "run_bash");
        assert_eq!(
            block["input"],
            serde_json::json!({"command": "echo hello"}),
            "aggregated partial json must be parsed into input so non-streaming clients see the arguments"
        );
        assert!(
            block.get("partial_json").is_none(),
            "streaming-only field must not leak into a non-streaming message"
        );
        assert_eq!(value["stop_reason"], "tool_use");
    }

    #[test]
    fn synthesized_message_keeps_complete_tool_use_input_untouched() {
        let stream = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"create\",\"input\":{\"filename\":\"a.txt\"}}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (body, had_stream_error) =
            synthesize_anthropic_message_json(stream.as_bytes()).expect("synthesize");
        assert!(!had_stream_error);
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            value["content"][0]["input"],
            serde_json::json!({"filename": "a.txt"})
        );
    }

    #[test]
    fn finalize_aggregated_tool_use_handles_degenerate_blocks() {
        // Unparseable fragments fall back to an empty object instead of a
        // string-typed input, and non-tool blocks are left alone.
        let mut broken = serde_json::json!(
            {"type": "tool_use", "id": "c", "name": "n", "input": {}, "partial_json": "{not-json"}
        );
        finalize_aggregated_tool_use(&mut broken);
        assert_eq!(broken["input"], serde_json::json!({}));
        assert!(broken.get("partial_json").is_none());

        let mut text = serde_json::json!({"type": "text", "text": "", "partial_json": "{}"});
        finalize_aggregated_tool_use(&mut text);
        assert_eq!(
            text["partial_json"], "{}",
            "only tool_use blocks are finalized"
        );
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
    async fn upstream_websocket_401_fails_fast_with_safe_auth_error() {
        async fn unauthorized_upstream() -> HttpResponse {
            // The proxy must not expose an upstream response body, which may
            // contain provider-specific diagnostic data.
            HttpResponse::Unauthorized().body("provider diagnostic: do not forward")
        }

        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream addr")
        );
        let upstream_server = HttpServer::new(|| {
            App::new().route("/v1/responses", web::get().to(unauthorized_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let request = format!(
            "{}/v1/responses",
            websocket_url(&upstream).expect("WebSocket URL")
        )
        .into_client_request()
        .expect("build upstream WebSocket request");
        let connect_started = Instant::now();
        let error = tokio::time::timeout(
            Duration::from_millis(500),
            connect_local_proxy_upstream_websocket(request, |_| {}),
        )
        .await
        .expect("401 should not enter the retry window")
        .expect_err("401 should reject the WebSocket handshake");
        assert!(
            connect_started.elapsed() < Duration::from_millis(500),
            "401 should fail fast rather than wait for the retry deadline"
        );
        assert!(matches!(
            &error,
            LocalProxyUpstreamWebSocketError::PermanentHandshake {
                upstream_status: 401
            }
        ));

        let payload = websocket_connect_error_payload(&error);
        assert_eq!(payload["status"], 401);
        assert_eq!(payload["error"]["type"], "authentication_error");
        assert_eq!(payload["error"]["code"], "upstream_authentication_failed");
        assert!(payload["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("Restart Codex"));
        assert!(
            !payload.to_string().contains("provider diagnostic"),
            "upstream response bodies must remain private"
        );

        // A rejected WebSocket handshake can leave the mock HTTP connection
        // counted as active while its error response is being torn down. A
        // graceful stop can then wait indefinitely under parallel CI load,
        // even though this test has already observed the complete response.
        upstream_handle.stop(false).await;
    }

    #[actix_web::test]
    async fn upstream_websocket_5xx_retries_until_the_handshake_succeeds() {
        async fn temporarily_unavailable_upstream(
            req: HttpRequest,
            payload: web::Payload,
            attempts: web::Data<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(HttpResponse::ServiceUnavailable().finish());
            }

            let (response, _session, _stream) = actix_ws::handle(&req, payload)?;
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
                .route(
                    "/v1/responses",
                    web::get().to(temporarily_unavailable_upstream),
                )
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let request = format!(
            "{}/v1/responses",
            websocket_url(&upstream).expect("WebSocket URL")
        )
        .into_client_request()
        .expect("build upstream WebSocket request");
        let websocket = tokio::time::timeout(
            Duration::from_secs(1),
            connect_local_proxy_upstream_websocket(request, |_| {}),
        )
        .await
        .expect("transient 5xx should retry before the retry deadline")
        .expect("second handshake should succeed");
        drop(websocket);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "a 503 handshake response should be retried"
        );

        upstream_handle.stop(true).await;
    }

    #[test]
    fn upstream_websocket_520_remains_retryable_and_reports_gateway_guidance() {
        assert!(
            is_permanent_upstream_websocket_handshake_status(407),
            "proxy authentication cannot recover through deployment retries"
        );
        assert!(
            !is_permanent_upstream_websocket_handshake_status(520),
            "opaque upstream gateway failures must remain retryable"
        );

        let response = tungstenite::http::Response::builder()
            .status(520)
            .body(Some(Vec::from("provider diagnostic: do not forward")))
            .expect("build 520 response");
        let error =
            LocalProxyUpstreamWebSocketError::from_tungstenite(tungstenite::Error::Http(response));

        assert!(matches!(
            &error,
            LocalProxyUpstreamWebSocketError::Unavailable {
                last_handshake_status: Some(520),
                ..
            }
        ));
        let payload = websocket_connect_error_payload(&error);
        assert_eq!(payload["status"], 502);
        assert_eq!(payload["error"]["code"], "upstream_gateway_error");
        assert_eq!(payload["error"]["upstream_status"], 520);
        assert!(payload["error"]["message"]
            .as_str()
            .expect("gateway error message")
            .contains("provider authentication"));
        assert!(
            !payload.to_string().contains("provider diagnostic"),
            "upstream response bodies must remain private"
        );
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
        // Dropping the session must actually tear the connection down so the
        // proxy can observe the idle drop. actix-http >= 3.13 lingers for the
        // server's client_disconnect_timeout (1s by default) after a WebSocket
        // body ends with an unfinished request payload, silently discarding
        // frames; the proxy then only learns the upstream is gone after it has
        // forwarded the client's message, which correctly fails that request
        // without replay. Disabling the linger restores the abrupt-drop
        // fixture this test is about on both actix-http lines.
        .client_disconnect_timeout(Duration::ZERO)
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

        // Close the active client connection before waiting for the blocking
        // proxy runtime to stop. Otherwise graceful server shutdown can wait
        // indefinitely for this test-owned WebSocket under parallel CI load.
        drop(ws);
        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_keeps_local_websocket_open_until_deployed_upstream_returns() {
        async fn echo_upstream(
            req: HttpRequest,
            payload: web::Payload,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            actix_web::rt::spawn(async move {
                let mut stream = stream.aggregate_continuations();
                if let Some(Ok(AggregatedMessage::Text(text))) = stream.next().await {
                    let _ = session.text(text.to_string()).await;
                }
            });
            Ok(response)
        }

        let upstream_port = allocate_proxy_test_port();
        let upstream = format!("http://127.0.0.1:{upstream_port}");
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
        ws.send(TungsteniteMessage::Text("during-deployment".into()))
            .await
            .expect("local WebSocket should remain writable while upstream is unavailable");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let upstream_listener =
            std::net::TcpListener::bind(("127.0.0.1", upstream_port)).expect("bind upstream");
        let upstream_server = HttpServer::new(move || {
            App::new().route("/v1/responses", web::get().to(echo_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("timed out waiting for recovered upstream")
            .expect("local WebSocket should remain open")
            .expect("recovered frame should read");
        match msg {
            TungsteniteMessage::Text(text) => assert_eq!(text, "during-deployment"),
            other => panic!("expected echoed WebSocket text frame, got {other:?}"),
        }

        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_recovers_next_turn_after_bounded_upstream_outage_without_replaying_failed_turn()
    {
        async fn echo_upstream(
            req: HttpRequest,
            payload: web::Payload,
            received: web::Data<std::sync::mpsc::Sender<String>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            let received = received.get_ref().clone();
            actix_web::rt::spawn(async move {
                let mut stream = stream.aggregate_continuations();
                if let Some(Ok(AggregatedMessage::Text(text))) = stream.next().await {
                    let text = text.to_string();
                    let _ = received.send(text.clone());
                    let _ = session.text(text).await;
                }
            });
            Ok(response)
        }

        // Leave this port without a listener until the first bounded retry
        // window has elapsed. The first client frame must receive an error,
        // but the local WebSocket must stay usable for the later frame.
        let upstream_port = allocate_proxy_test_port();
        let upstream = format!("http://127.0.0.1:{upstream_port}");
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

        ws.send(TungsteniteMessage::Text("failed-turn".into()))
            .await
            .expect("failed turn should reach the local proxy");
        let failure = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("bounded upstream retry should produce an error event")
            .expect("local WebSocket must remain open after failed turn")
            .expect("failed-turn error frame should read");
        let TungsteniteMessage::Text(failure) = failure else {
            panic!("expected a failed-turn error text frame, got {failure:?}");
        };
        let failure: serde_json::Value =
            serde_json::from_str(&failure).expect("failed-turn error should be JSON");
        assert_eq!(failure["type"], "error");
        assert_eq!(failure["status"], 502);

        let (received_tx, received_rx) = mpsc::channel::<String>();
        let upstream_listener =
            std::net::TcpListener::bind(("127.0.0.1", upstream_port)).expect("bind upstream");
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(received_tx.clone()))
                .route("/v1/responses", web::get().to(echo_upstream))
        })
        .listen(upstream_listener)
        .expect("listen upstream")
        .run();
        let upstream_handle = upstream_server.handle();
        actix_web::rt::spawn(upstream_server);

        ws.send(TungsteniteMessage::Text("next-turn".into()))
            .await
            .expect("next turn should be accepted on the same local WebSocket");
        let recovered = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("next turn should reconnect to the recovered upstream")
            .expect("local WebSocket should remain open after recovery")
            .expect("recovered frame should read");
        match recovered {
            TungsteniteMessage::Text(text) => assert_eq!(text, "next-turn"),
            other => panic!("expected echoed next-turn frame, got {other:?}"),
        }
        assert_eq!(
            received_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("recovered upstream should receive next turn"),
            "next-turn"
        );
        assert!(
            received_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "failed turn must not be replayed after recovery"
        );

        drop(ws);
        let _ = shutdown_tx.send(());
        proxy_thread.join().expect("proxy thread should not panic");
        upstream_handle.stop(true).await;
    }

    #[actix_web::test]
    async fn proxy_recovers_after_in_flight_upstream_drop_without_replaying_turn() {
        async fn upstream_that_drops_first_turn(
            req: HttpRequest,
            payload: web::Payload,
            attempts: web::Data<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
            received: web::Data<std::sync::mpsc::Sender<String>>,
        ) -> Result<HttpResponse, actix_web::Error> {
            let (response, mut session, stream) = actix_ws::handle(&req, payload)?;
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            let received = received.get_ref().clone();
            actix_web::rt::spawn(async move {
                let mut stream = stream.aggregate_continuations();
                if let Some(Ok(AggregatedMessage::Text(text))) = stream.next().await {
                    let text = text.to_string();
                    let _ = received.send(text.clone());
                    if attempt == 0 {
                        // The proxy cannot know whether this first turn made
                        // it far enough upstream to have side effects. Drop
                        // the connection without a terminal response.
                        drop(session);
                    } else {
                        let _ = session.text(text).await;
                    }
                }
            });
            Ok(response)
        }

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (received_tx, received_rx) = mpsc::channel::<String>();
        let upstream_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream port");
        let upstream = format!(
            "http://{}",
            upstream_listener.local_addr().expect("upstream address")
        );
        let attempts_for_server = attempts.clone();
        let upstream_server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(attempts_for_server.clone()))
                .app_data(web::Data::new(received_tx.clone()))
                .route(
                    "/v1/responses",
                    web::get().to(upstream_that_drops_first_turn),
                )
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

        ws.send(TungsteniteMessage::Text("failed-turn".into()))
            .await
            .expect("first turn should reach the local proxy");
        let failure = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("in-flight upstream loss should fail the current turn")
            .expect("local WebSocket must remain open after in-flight loss")
            .expect("in-flight failure frame should read");
        let TungsteniteMessage::Text(failure) = failure else {
            panic!("expected an in-flight failure error frame, got {failure:?}");
        };
        let failure: serde_json::Value =
            serde_json::from_str(&failure).expect("in-flight failure should be JSON");
        assert_eq!(failure["type"], "error");
        assert_eq!(failure["status"], 502);

        ws.send(TungsteniteMessage::Text("next-turn".into()))
            .await
            .expect("next turn should be accepted on the same local WebSocket");
        let recovered = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("next turn should reconnect after the in-flight loss")
            .expect("local WebSocket should remain open after recovery")
            .expect("recovered frame should read");
        match recovered {
            TungsteniteMessage::Text(text) => assert_eq!(text, "next-turn"),
            other => panic!("expected echoed next-turn frame, got {other:?}"),
        }
        assert_eq!(
            received_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("upstream should receive the failed turn once"),
            "failed-turn"
        );
        assert_eq!(
            received_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("upstream should receive the recovered turn"),
            "next-turn"
        );
        assert!(
            received_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "the ambiguous failed turn must not be replayed"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "only the new turn should establish the replacement upstream connection"
        );

        drop(ws);
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
