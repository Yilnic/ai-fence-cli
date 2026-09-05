//! OIDC device authorization flow for CLI authentication.
//!
//! Implements RFC 8628 device authorization grant using raw HTTP requests,
//! allowing users to authenticate with an OIDC provider (e.g., Keycloak) via
//! a browser-based flow.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::info;

static NEXT_ATOMIC_WRITE_ID: AtomicU64 = AtomicU64::new(1);

/// OIDC discovery document.
#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    device_authorization_endpoint: String,
    token_endpoint: String,
}

/// Device authorization response.
#[derive(Debug, Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    error_uri: Option<String>,
}

impl OAuthErrorResponse {
    fn message(self) -> String {
        let error = self.error.unwrap_or_else(|| "unknown_error".to_string());
        match (self.error_description, self.error_uri) {
            (Some(description), Some(uri)) => format!("{error}: {description} ({uri})"),
            (Some(description), None) => format!("{error}: {description}"),
            (None, Some(uri)) => format!("{error} ({uri})"),
            (None, None) => error,
        }
    }
}

/// Token response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    #[allow(dead_code)]
    token_type: String,
}

const DEVICE_AUTH_SCOPE: &str = "openid profile email offline_access";

/// Stored OIDC credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>, // Unix timestamp
    pub issuer: String,
    pub client_id: String,
}

impl StoredCredentials {
    /// Check if the access token has expired (with 60s buffer).
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = chrono::Utc::now().timestamp();
                now >= exp - 60
            }
            None => true,
        }
    }
}

/// Get the credentials file path.
pub fn credentials_path() -> Result<PathBuf> {
    let dir = crate::config::config_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("credentials.json"))
}

pub fn api_key_path() -> Result<PathBuf> {
    let dir = crate::config::config_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("api-key"))
}

/// Load stored credentials from disk.
pub fn load_credentials() -> Result<Option<StoredCredentials>> {
    let path = credentials_path()?;
    load_credentials_from_path(&path)
}

pub(crate) fn load_credentials_from_path(path: &Path) -> Result<Option<StoredCredentials>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let creds: StoredCredentials = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(creds))
}

/// Save credentials to disk.
pub fn save_credentials(creds: &StoredCredentials) -> Result<()> {
    let path = credentials_path()?;
    save_credentials_to_path(&path, creds)
}

fn save_credentials_to_path(path: &Path, creds: &StoredCredentials) -> Result<()> {
    let content = serde_json::to_string_pretty(creds)?;
    atomic_write_private(path, content.as_bytes())
}

/// Remove stored credentials from disk.
pub fn delete_credentials() -> Result<()> {
    let path = credentials_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete {}", path.display()))?;
    }
    Ok(())
}

pub fn load_api_key() -> Result<Option<String>> {
    let path = api_key_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let key = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?
        .trim()
        .to_string();
    if key.is_empty() {
        Ok(None)
    } else {
        Ok(Some(key))
    }
}

pub fn save_api_key(key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("API key is empty");
    }
    let path = api_key_path()?;
    let mut content = key.as_bytes().to_vec();
    content.push(b'\n');
    atomic_write_private(&path, &content)
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("credential path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("credentials");
    let write_id = NEXT_ATOMIC_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary_path = parent.join(format!(
        ".{file_name}.tmp-{}-{write_id}",
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary_path).with_context(|| {
            format!(
                "Failed to open temporary credential file {}",
                temporary_path.display()
            )
        })?;
        file.write_all(content).with_context(|| {
            format!(
                "Failed to write temporary credential file {}",
                temporary_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to sync temporary credential file {}",
                temporary_path.display()
            )
        })?;
        drop(file);

        #[cfg(windows)]
        {
            if let Err(rename_error) = std::fs::rename(&temporary_path, path) {
                std::fs::remove_file(path).with_context(|| {
                    format!(
                        "Failed to replace existing credential file {}",
                        path.display()
                    )
                })?;
                std::fs::rename(&temporary_path, path).with_context(|| {
                    format!(
                        "Failed to rename temporary credential file after {rename_error}: {}",
                        path.display()
                    )
                })?;
            }
        }
        #[cfg(not(windows))]
        std::fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "Failed to atomically replace credential file {}",
                path.display()
            )
        })?;

        #[cfg(unix)]
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

pub fn delete_api_key() -> Result<()> {
    let path = api_key_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to delete {}", path.display()))?;
    }
    Ok(())
}

/// Run the OIDC device authorization flow.
///
/// 1. Fetches the OIDC discovery document
/// 2. Requests device authorization
/// 3. Displays verification URL and user code
/// 4. Polls the token endpoint until the user completes the flow
pub async fn device_auth_login(issuer: &str, client_id: &str) -> Result<StoredCredentials> {
    let http = reqwest::Client::new();

    // 1. Fetch discovery document
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let discovery: OidcDiscovery = http
        .get(&discovery_url)
        .send()
        .await
        .context("Failed to fetch OIDC discovery document")?
        .json()
        .await
        .context("Failed to parse OIDC discovery document")?;

    info!(
        device_auth_endpoint = %discovery.device_authorization_endpoint,
        token_endpoint = %discovery.token_endpoint,
        "OIDC discovery successful"
    );

    // 2. Request device authorization
    let device_response = http
        .post(&discovery.device_authorization_endpoint)
        .form(&[
            ("client_id", client_id),
            ("scope", device_authorization_scope()),
        ])
        .send()
        .await
        .context("Device authorization request failed")?;
    if !device_response.status().is_success() {
        let status = device_response.status();
        let error = device_response
            .json::<OAuthErrorResponse>()
            .await
            .map(OAuthErrorResponse::message)
            .unwrap_or_else(|_| "failed to parse OAuth error response".to_string());
        anyhow::bail!("Device authorization request returned {status}: {error}");
    }
    let device_resp: DeviceAuthResponse = device_response
        .json()
        .await
        .context("Failed to parse device authorization response")?;

    println!();
    println!("To authenticate, visit:");
    if let Some(uri) = &device_resp.verification_uri_complete {
        println!("  {uri}");
    } else {
        println!("  {}", device_resp.verification_uri);
    }
    println!();
    println!("And enter the code:");
    println!("  {}", device_resp.user_code);
    println!();
    println!("Waiting for authentication...");

    // 3. Poll for the token
    let poll_interval = device_resp.interval.unwrap_or(5);
    let timeout = device_resp.expires_in;

    let token = poll_for_token(
        &http,
        &discovery.token_endpoint,
        client_id,
        &device_resp.device_code,
        poll_interval,
        timeout,
    )
    .await?;

    let creds = StoredCredentials {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token
            .expires_in
            .map(|d| chrono::Utc::now().timestamp() + d as i64),
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
    };

    save_credentials(&creds)?;
    println!("Authentication successful. Credentials saved.");
    Ok(creds)
}

/// Refresh an expired OIDC access token and atomically persist the new
/// credentials. Providers may rotate the refresh token, but RFC 6749 also
/// permits omitting it; in that case the previous refresh token remains
/// usable for the next refresh.
pub async fn refresh_credentials(
    _issuer: &str,
    _client_id: &str,
    existing: &StoredCredentials,
) -> Result<StoredCredentials> {
    let path = credentials_path()?;
    refresh_credentials_after_rejection_from_path(&path, &existing.access_token).await
}

/// Return usable credentials, refreshing them when they are expired or close
/// to expiry. A filesystem lock serializes refresh-token rotation across all
/// local CLI/proxy processes that share this credential file.
pub async fn ensure_fresh_credentials() -> Result<StoredCredentials> {
    let path = credentials_path()?;
    ensure_fresh_credentials_from_path(&path).await
}

pub(crate) async fn ensure_fresh_credentials_from_path(path: &Path) -> Result<StoredCredentials> {
    let credentials = required_credentials_from_path(path)?;
    if !credentials.is_expired() {
        return Ok(credentials);
    }

    refresh_credentials_from_path(path, RefreshReason::Expired).await
}

/// Refresh credentials after the backend rejected the exact access token used
/// for a request. If another process already replaced that token, reuse the
/// replacement instead of spending or invalidating the rotated refresh token.
pub(crate) async fn refresh_credentials_after_rejection_from_path(
    path: &Path,
    rejected_access_token: &str,
) -> Result<StoredCredentials> {
    let credentials = required_credentials_from_path(path)?;
    if credentials.access_token != rejected_access_token && !credentials.is_expired() {
        return Ok(credentials);
    }

    refresh_credentials_from_path(path, RefreshReason::Rejected(rejected_access_token)).await
}

enum RefreshReason<'a> {
    Expired,
    Rejected(&'a str),
}

async fn refresh_credentials_from_path(
    path: &Path,
    reason: RefreshReason<'_>,
) -> Result<StoredCredentials> {
    let _lock = acquire_credentials_refresh_lock(path).await?;
    let current = required_credentials_from_path(path)?;
    match reason {
        RefreshReason::Expired if !current.is_expired() => return Ok(current),
        RefreshReason::Rejected(rejected)
            if current.access_token != rejected && !current.is_expired() =>
        {
            return Ok(current);
        }
        RefreshReason::Expired | RefreshReason::Rejected(_) => {}
    }

    let refreshed = request_refreshed_credentials(&current).await?;
    save_credentials_to_path(path, &refreshed)?;
    Ok(refreshed)
}

fn required_credentials_from_path(path: &Path) -> Result<StoredCredentials> {
    load_credentials_from_path(path)?.with_context(|| {
        format!(
            "No stored OIDC credentials at {}. Run `ai-fence-cli login` first.",
            path.display()
        )
    })
}

async fn acquire_credentials_refresh_lock(path: &Path) -> Result<std::fs::File> {
    let lock_path = credentials_refresh_lock_path(path)?;
    tokio::task::spawn_blocking(move || {
        let parent = lock_path
            .parent()
            .context("credential refresh lock path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path).with_context(|| {
            format!(
                "Failed to open OIDC credential refresh lock {}",
                lock_path.display()
            )
        })?;
        lock.lock_exclusive().with_context(|| {
            format!(
                "Failed to lock OIDC credential refresh lock {}",
                lock_path.display()
            )
        })?;
        Ok::<_, anyhow::Error>(lock)
    })
    .await
    .context("OIDC credential refresh lock task failed")?
}

fn credentials_refresh_lock_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("credential path has no parent directory")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("credentials.json");
    Ok(parent.join(format!(".{file_name}.refresh.lock")))
}

async fn request_refreshed_credentials(existing: &StoredCredentials) -> Result<StoredCredentials> {
    let refresh_token = existing
        .refresh_token
        .as_deref()
        .context("stored OIDC credentials do not contain a refresh token")?;
    let http = reqwest::Client::new();
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        existing.issuer.trim_end_matches('/')
    );
    let discovery: OidcDiscovery = http
        .get(&discovery_url)
        .send()
        .await
        .context("Failed to fetch OIDC discovery document for refresh")?
        .json()
        .await
        .context("Failed to parse OIDC discovery document for refresh")?;

    let response = http
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", existing.client_id.as_str()),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("OIDC refresh request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let error = response
            .json::<OAuthErrorResponse>()
            .await
            .map(OAuthErrorResponse::message)
            .unwrap_or_else(|_| "failed to parse OAuth refresh error response".to_string());
        anyhow::bail!("OIDC refresh request returned {status}: {error}");
    }

    let token: TokenResponse = response
        .json()
        .await
        .context("Failed to parse OIDC refresh token response")?;
    refreshed_credentials(existing, token)
}

fn device_authorization_scope() -> &'static str {
    DEVICE_AUTH_SCOPE
}

fn refreshed_credentials(
    existing: &StoredCredentials,
    token: TokenResponse,
) -> Result<StoredCredentials> {
    if token.access_token.trim().is_empty() {
        anyhow::bail!("OIDC refresh response contained an empty access token");
    }
    let expires_in = token
        .expires_in
        .filter(|expires_in| *expires_in > 60)
        .context("OIDC refresh response did not contain a usable expiration")?;
    Ok(StoredCredentials {
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .or_else(|| existing.refresh_token.clone()),
        expires_at: Some(chrono::Utc::now().timestamp() + expires_in as i64),
        issuer: existing.issuer.clone(),
        client_id: existing.client_id.clone(),
    })
}

/// Poll the token endpoint until the user completes the flow or timeout.
async fn poll_for_token(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    device_code: &str,
    poll_interval: u64,
    timeout_secs: u64,
) -> Result<TokenResponse> {
    let start = Instant::now();

    loop {
        tokio::time::sleep(Duration::from_secs(poll_interval)).await;

        if start.elapsed() > Duration::from_secs(timeout_secs) {
            anyhow::bail!("Device authorization timed out after {timeout_secs}s");
        }

        let resp = http
            .post(token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", device_code),
            ])
            .send()
            .await
            .context("Token request failed")?;

        if resp.status().is_success() {
            let token: TokenResponse = resp
                .json()
                .await
                .context("Failed to parse token response")?;
            return Ok(token);
        }

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let error = body["error"].as_str().unwrap_or("unknown");

        match error {
            "authorization_pending" | "slow_down" => {
                // Expected — keep polling
                if error == "slow_down" {
                    // Double the interval temporarily
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                continue;
            }
            "access_denied" => {
                anyhow::bail!("Device authorization was denied by the user");
            }
            "expired_token" => {
                anyhow::bail!("Device authorization code expired. Please try again.");
            }
            _ => {
                tracing::debug!(status = %status, error = error, "Token poll error (continuing)");
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn spawn_refresh_server() -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind refresh server");
        let address = listener.local_addr().expect("refresh server address");
        let base_url = format!("http://{address}");
        let discovery_calls = Arc::new(AtomicUsize::new(0));
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let discovery_calls_for_server = Arc::clone(&discovery_calls);
        let refresh_calls_for_server = Arc::clone(&refresh_calls);
        let token_endpoint = format!("{base_url}/token");
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept refresh request");
                let mut request = vec![0_u8; 8192];
                let bytes_read = stream
                    .read(&mut request)
                    .await
                    .expect("read refresh request");
                let request = String::from_utf8_lossy(&request[..bytes_read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default();
                let body = if path == "/.well-known/openid-configuration" {
                    discovery_calls_for_server.fetch_add(1, Ordering::SeqCst);
                    serde_json::json!({
                        "device_authorization_endpoint": format!("{token_endpoint}/device"),
                        "token_endpoint": token_endpoint,
                    })
                    .to_string()
                } else if path == "/token" {
                    refresh_calls_for_server.fetch_add(1, Ordering::SeqCst);
                    serde_json::json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "expires_in": 3600,
                        "token_type": "Bearer",
                    })
                    .to_string()
                } else {
                    panic!("unexpected refresh server path: {path}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write refresh response");
            }
        });
        (base_url, discovery_calls, refresh_calls, server)
    }

    fn write_credentials_at(path: &Path, credentials: &StoredCredentials) {
        let content = serde_json::to_vec_pretty(credentials).expect("serialize credentials");
        atomic_write_private(path, &content).expect("write credentials");
    }

    #[test]
    fn device_authorization_requests_refresh_capability() {
        assert!(device_authorization_scope()
            .split_whitespace()
            .any(|scope| scope == "offline_access"));
    }

    #[test]
    fn refresh_response_preserves_refresh_token_when_provider_does_not_rotate_it() {
        let existing = StoredCredentials {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: Some(chrono::Utc::now().timestamp() - 60),
            issuer: "https://issuer.example".to_string(),
            client_id: "ai-fence-cli".to_string(),
        };
        let token = TokenResponse {
            access_token: "new-access".to_string(),
            refresh_token: None,
            expires_in: Some(3600),
            token_type: "Bearer".to_string(),
        };

        let refreshed = refreshed_credentials(&existing, token)
            .expect("refresh response should produce valid credentials");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("old-refresh"));
        assert!(!refreshed.is_expired());
    }

    #[actix_web::test]
    async fn concurrent_expired_refresh_is_single_flight_across_the_credential_file() {
        let temp = tempfile::tempdir().expect("credential directory");
        let path = temp.path().join("credentials.json");
        let (issuer, discovery_calls, refresh_calls, server) = spawn_refresh_server().await;
        write_credentials_at(
            &path,
            &StoredCredentials {
                access_token: "old-access".to_string(),
                refresh_token: Some("old-refresh".to_string()),
                expires_at: Some(chrono::Utc::now().timestamp() - 60),
                issuer,
                client_id: "ai-fence-cli".to_string(),
            },
        );

        let first_path = path.clone();
        let second_path = path.clone();
        let (first, second) = tokio::join!(
            ensure_fresh_credentials_from_path(&first_path),
            ensure_fresh_credentials_from_path(&second_path),
        );

        assert_eq!(first.expect("first refresh").access_token, "new-access");
        assert_eq!(second.expect("second refresh").access_token, "new-access");
        assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
        let persisted = load_credentials_from_path(&path)
            .expect("load refreshed credentials")
            .expect("refreshed credentials");
        assert_eq!(persisted.access_token, "new-access");
        assert_eq!(persisted.refresh_token.as_deref(), Some("new-refresh"));
        server.abort();
    }

    #[actix_web::test]
    async fn rejected_old_access_token_reuses_a_newer_file_without_refreshing_again() {
        let temp = tempfile::tempdir().expect("credential directory");
        let path = temp.path().join("credentials.json");
        write_credentials_at(
            &path,
            &StoredCredentials {
                access_token: "new-access".to_string(),
                refresh_token: Some("new-refresh".to_string()),
                expires_at: Some(chrono::Utc::now().timestamp() + 3600),
                issuer: "http://refresh-must-not-run.invalid".to_string(),
                client_id: "ai-fence-cli".to_string(),
            },
        );

        let credentials = refresh_credentials_after_rejection_from_path(&path, "old-access")
            .await
            .expect("newer credentials should be reused");

        assert_eq!(credentials.access_token, "new-access");
        assert_eq!(credentials.refresh_token.as_deref(), Some("new-refresh"));
    }
}
