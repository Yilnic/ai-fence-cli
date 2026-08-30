//! OIDC device authorization flow for CLI authentication.
//!
//! Implements RFC 8628 device authorization grant using raw HTTP requests,
//! allowing users to authenticate with an OIDC provider (e.g., Keycloak) via
//! a browser-based flow.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::info;

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
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let creds: StoredCredentials = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(creds))
}

/// Save credentials to disk.
pub fn save_credentials(creds: &StoredCredentials) -> Result<()> {
    let path = credentials_path()?;
    let content = serde_json::to_string_pretty(creds)?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
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
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.write_all(key.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
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
        .form(&[("client_id", client_id), ("scope", "openid profile email")])
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
