//! Shared connection transport; never owns an application's business jobs.
use ai_fence_contract::application::{
    ApplicationBinding, ApplicationCapabilities, BindingIdentity, ManagedConnection,
    ReconcileBindingRequest, ReconcileBindingResponse,
};
use reqwest::{Client, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::{
    fmt,
    time::{Duration, Instant},
};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
mod application_cancellations;
mod application_offers;
mod application_revocations;
mod application_runs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    Transport,
    Http(u16),
    ResponseTooLarge,
    InvalidResponse,
    BindingMismatch,
    ExpiredCredential,
    RunMismatch,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(status) => write!(f, "AI Fence request rejected ({status})"),
            other => f.write_str(match other {
                Self::InvalidConfiguration => "invalid AI Fence connection configuration",
                Self::Transport => "AI Fence transport unavailable",
                Self::ResponseTooLarge => "AI Fence response exceeds limit",
                Self::InvalidResponse => "invalid AI Fence response",
                Self::BindingMismatch => {
                    "AI Fence binding does not match the configured connection"
                }
                Self::ExpiredCredential => "AI Fence service credential has expired",
                Self::RunMismatch => "AI Fence run does not match the requested attempt",
                Self::Http(_) => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for Error {}

// These types deliberately have no Debug/Serialize or public value accessor.
pub struct ManagementCredential(String);
pub struct ClientSecret(String);
pub struct ServiceCredential {
    token: String,
    expires: Instant,
    identity: CredentialIdentity,
}

#[derive(PartialEq, Eq)]
struct CredentialIdentity {
    fence_origin: String,
    binding: BindingIdentity,
    issuer: String,
    client_id: String,
    audience: String,
    scope: String,
}

impl CredentialIdentity {
    fn capture(origin: &Url, connection: &ManagedConnection) -> Self {
        Self {
            fence_origin: origin.origin().ascii_serialization(),
            binding: connection.binding.clone(),
            issuer: connection.auth.issuer.clone(),
            client_id: connection.auth.client_id.clone(),
            audience: connection.auth.audience.clone(),
            scope: connection.auth.scope.clone(),
        }
    }
}

fn valid_secret(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 * 1024 && !value.chars().any(char::is_control)
}
impl ManagementCredential {
    pub fn new(value: String) -> Result<Self, Error> {
        if !valid_secret(&value) || value.chars().any(char::is_whitespace) {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self(value))
    }
}
impl ClientSecret {
    pub fn new(value: String) -> Result<Self, Error> {
        if !valid_secret(&value) {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self(value))
    }
}

#[derive(Clone)]
pub struct ApplicationClient {
    origin: Url,
    http: Client,
}

fn secure_url(value: &str) -> Result<Url, Error> {
    let url = Url::parse(value).map_err(|_| Error::InvalidConfiguration)?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::InvalidConfiguration);
    }
    Ok(url)
}

impl ApplicationClient {
    pub fn new(fence_url: &str, timeout: Duration) -> Result<Self, Error> {
        let origin = secure_url(fence_url)?;
        if origin.path() != "/" || timeout.is_zero() || timeout > Duration::from_secs(120) {
            return Err(Error::InvalidConfiguration);
        }
        let http = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| Error::InvalidConfiguration)?;
        Ok(Self { origin, http })
    }

    fn endpoint(&self, path: &str) -> Result<Url, Error> {
        self.origin
            .join(path)
            .map_err(|_| Error::InvalidConfiguration)
    }

    pub async fn reconcile_binding(
        &self,
        id: &str,
        request: &ReconcileBindingRequest,
        credential: &ManagementCredential,
    ) -> Result<ReconcileBindingResponse, Error> {
        request
            .spec
            .validate()
            .map_err(|_| Error::InvalidConfiguration)?;
        if request.schema_version != 1
            || id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .put(self.endpoint(&format!("/api/v1/internal/application-bindings/{id}"))?)
            .bearer_auth(&credential.0)
            .json(request)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value: ReconcileBindingResponse = read_json(response).await?;
        value
            .binding
            .validate()
            .map_err(|_| Error::InvalidResponse)?;
        if value.binding.binding_id != id || value.binding.spec != request.spec {
            return Err(Error::BindingMismatch);
        }
        Ok(value)
    }

    pub async fn binding(
        &self,
        id: &str,
        credential: &ManagementCredential,
    ) -> Result<ApplicationBinding, Error> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(Error::InvalidConfiguration);
        }
        let response = self
            .http
            .get(self.endpoint(&format!("/api/v1/internal/application-bindings/{id}"))?)
            .bearer_auth(&credential.0)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value: ApplicationBinding = read_json(response).await?;
        value.validate().map_err(|_| Error::InvalidResponse)?;
        if value.binding_id != id {
            return Err(Error::BindingMismatch);
        }
        Ok(value)
    }

    fn validate_connection(&self, connection: &ManagedConnection) -> Result<(), Error> {
        connection
            .validate()
            .map_err(|_| Error::InvalidConfiguration)?;
        let fence = secure_url(&connection.fence_url)?;
        let issuer = secure_url(&connection.auth.issuer)?;
        let token = secure_url(&connection.auth.token_endpoint)?;
        if fence.origin() != self.origin.origin()
            || fence.path() != "/"
            || token.origin() != issuer.origin()
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(())
    }

    pub async fn authenticate(
        &self,
        connection: &ManagedConnection,
        secret: &ClientSecret,
    ) -> Result<ServiceCredential, Error> {
        self.validate_connection(connection)?;
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            token_type: String,
            expires_in: u64,
        }
        let started = Instant::now();
        let response = self
            .http
            .post(&connection.auth.token_endpoint)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", connection.auth.client_id.as_str()),
                ("client_secret", secret.0.as_str()),
                ("scope", connection.auth.scope.as_str()),
            ])
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let token: Token = read_json(response).await?;
        if !valid_secret(&token.access_token)
            || token.access_token.chars().any(char::is_whitespace)
            || !token.token_type.eq_ignore_ascii_case("Bearer")
            || token.expires_in == 0
            || token.expires_in > 86400
        {
            return Err(Error::InvalidResponse);
        }
        let lifetime = Duration::from_secs(token.expires_in);
        if started.elapsed() >= lifetime {
            return Err(Error::ExpiredCredential);
        }
        Ok(ServiceCredential {
            token: token.access_token,
            expires: started + lifetime,
            identity: CredentialIdentity::capture(&self.origin, connection),
        })
    }

    fn validate_service_credential(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
    ) -> Result<(), Error> {
        self.validate_connection(connection)?;
        if Instant::now() >= credential.expires {
            return Err(Error::ExpiredCredential);
        }
        if credential.identity != CredentialIdentity::capture(&self.origin, connection) {
            return Err(Error::BindingMismatch);
        }
        Ok(())
    }

    pub async fn capabilities(
        &self,
        connection: &ManagedConnection,
        credential: &ServiceCredential,
    ) -> Result<ApplicationCapabilities, Error> {
        self.validate_service_credential(connection, credential)?;
        let response = self
            .http
            .get(self.endpoint("/api/v1/application-bindings/self/capabilities")?)
            .bearer_auth(&credential.token)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        let value: ApplicationCapabilities = read_json(response).await?;
        if !value.matches(connection) {
            return Err(Error::BindingMismatch);
        }
        Ok(value)
    }
}

async fn read_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
    read_json_with_limit(response, MAX_RESPONSE_BYTES).await
}

async fn read_json_with_limit<T: DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, Error> {
    if response.status() != StatusCode::OK && response.status() != StatusCode::CREATED {
        return Err(Error::Http(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(Error::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if chunk.len() > limit - bytes.len() {
            return Err(Error::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)
}

#[cfg(test)]
mod tests;
