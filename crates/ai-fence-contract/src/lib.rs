use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Style of placeholder used when redacting sensitive data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaceholderStyle {
    Tagged,
    Neutral,
    Sentinel,
    #[default]
    Realistic,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GatewayMetadata {
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub execution_id: Option<String>,
    pub workspace_id: Option<String>,
    pub target_id: Option<String>,
    pub origin: Option<String>,
    pub profile: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub user_name: Option<String>,
    pub user_role: Option<String>,
    /// Trusted provenance for an inferred identity. Ordinary callers must not
    /// use this field to override authenticated request identity.
    pub user_attribution_source: Option<String>,
    pub project_id: Option<String>,
    pub source: Option<String>,
    pub message_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub credential_pool_id: Option<String>,
    pub rental_reason: Option<String>,
    /// Server-authored project policy evidence. Ordinary gateway callers
    /// cannot mint this metadata because policy-bound keys are created through
    /// the authenticated internal control-plane API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_binding: Option<GatewayPolicyBinding>,
}

/// Immutable policy evidence attached to a short-lived gateway key.
///
/// The allow-lists remain the enforcement representation used on every HTTP
/// and WebSocket request. This binding identifies the project policy/access
/// set that compiled those lists so the issuing control plane can reject a
/// mismatched handoff and audits can explain the decision later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayPolicyBinding {
    pub project_id: String,
    pub policy_version: i64,
    pub policy_digest: String,
    pub model_access_set_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_assurance: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CreateGatewayKeyRequest {
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub expires_in_seconds: Option<i64>,
    #[serde(default)]
    pub metadata: GatewayMetadata,
    #[serde(default = "default_allowed")]
    pub allowed_providers: Vec<String>,
    #[serde(default = "default_allowed")]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub budget: Option<Value>,
}

impl CreateGatewayKeyRequest {
    pub fn session(
        expires_in_seconds: u64,
        metadata: GatewayMetadata,
        allowed_providers: Vec<String>,
        allowed_models: Vec<String>,
    ) -> Self {
        Self {
            scope: "session".to_string(),
            expires_in_seconds: Some(expires_in_seconds.min(i64::MAX as u64) as i64),
            metadata,
            allowed_providers,
            allowed_models,
            budget: None,
        }
    }
}

fn default_scope() -> String {
    "session".to_string()
}

fn default_allowed() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateGatewayKeyResponse {
    pub key_id: String,
    pub secret: String,
    #[serde(default)]
    pub base_urls: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: Value,
    pub expires_at: Option<DateTime<Utc>>,
    /// Echo of the exact policy evidence persisted with the key. A control
    /// plane requesting a bound key must compare this before handing out the
    /// returned credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_binding: Option<GatewayPolicyBinding>,
}

impl CreateGatewayKeyResponse {
    pub fn bearer_handoff_value(&self) -> Value {
        serde_json::json!({
            "status": "configured",
            "key_id": self.key_id,
            "gateway_key": self.secret,
            "base_urls": self.base_urls,
            "headers": {
                "Authorization": format!("Bearer {}", self.secret),
            },
            "expires_at": self.expires_at,
            "policy_binding": self.policy_binding,
        })
    }

    pub fn metadata_value(&self) -> Value {
        serde_json::json!({
            "status": "configured",
            "key_id": self.key_id,
            "base_urls": self.base_urls,
            "expires_at": self.expires_at,
            "policy_binding": self.policy_binding,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayKeyInfo {
    pub key_id: String,
    pub scope: String,
    pub metadata: GatewayMetadata,
    pub allowed_providers: Vec<String>,
    pub allowed_models: Vec<String>,
    pub budget: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevokeGatewayKeyResponse {
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayBaseUrls {
    pub openai: String,
    pub anthropic: String,
}

impl GatewayBaseUrls {
    pub fn into_map(self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("openai".to_string(), self.openai),
            ("anthropic".to_string(), self.anthropic),
        ])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// True when the server reached its bounded evidence window and cannot
    /// prove that the returned page is the end of the collection.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct GatewayUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_estimate: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayProvider {
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "anthropic")]
    Anthropic,
}

impl GatewayProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "anthropic" => Self::Anthropic,
            _ => Self::OpenAI,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderEndpointType {
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
    #[serde(rename = "openai_chat_completions")]
    OpenAIChatCompletions,
    #[serde(rename = "openai_completions")]
    OpenAICompletions,
    #[serde(rename = "openai_embeddings")]
    OpenAIEmbeddings,
    #[serde(rename = "openai_compatible_chat")]
    OpenAICompatibleChat,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl ProviderEndpointType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAIResponses => "openai_responses",
            Self::OpenAIChatCompletions => "openai_chat_completions",
            Self::OpenAICompletions => "openai_completions",
            Self::OpenAIEmbeddings => "openai_embeddings",
            Self::OpenAICompatibleChat => "openai_compatible_chat",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "openai_chat_completions" => Self::OpenAIChatCompletions,
            "openai_completions" => Self::OpenAICompletions,
            "openai_embeddings" => Self::OpenAIEmbeddings,
            "openai_compatible_chat" => Self::OpenAICompatibleChat,
            "anthropic_messages" => Self::AnthropicMessages,
            _ => Self::OpenAIResponses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallStatus {
    Started,
    Completed,
    Failed,
}

impl ProviderCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Started,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedRole {
    System,
    User,
    Assistant,
    Tool,
    Event,
}

impl NormalizedRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Event => "event",
        }
    }

    pub fn from_provider_role(value: &str) -> Self {
        match value {
            "system" | "developer" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            _ => Self::Event,
        }
    }

    pub fn from_db(value: &str) -> Self {
        Self::from_provider_role(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderCallRecord {
    pub id: String,
    pub key_id: Option<String>,
    /// Server-generated relational key. `request_id` remains the caller-visible
    /// value and is intentionally not unique or trusted for joins.
    #[serde(default)]
    pub correlation_id: Option<String>,
    pub request_id: Option<String>,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub user_name: Option<String>,
    pub user_role: Option<String>,
    pub message_id: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    /// Opaque Responses protocol lineage. These identifiers are retained even
    /// when payload content retention is disabled.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default)]
    pub response_id: Option<String>,
    /// Trusted, opaque principal scope used to prevent cross-user response-ID
    /// replay or conversation inheritance.
    #[serde(default)]
    pub continuation_scope: Option<String>,
    pub execution_id: Option<String>,
    pub workspace_id: Option<String>,
    pub target_id: Option<String>,
    pub origin: Option<String>,
    pub profile: Option<String>,
    pub provider: GatewayProvider,
    pub model: String,
    pub target_model: Option<String>,
    pub endpoint_type: ProviderEndpointType,
    pub status: ProviderCallStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub streamed: bool,
    pub usage: Option<GatewayUsage>,
    pub error: Option<String>,
    pub raw_request: Option<Value>,
    pub transformed_request: Option<Value>,
    pub raw_response: Option<Value>,
    pub processed_response: Option<Value>,
    pub raw_stream: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedMessage {
    pub id: String,
    pub provider_call_id: String,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub role: NormalizedRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub metadata: Value,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderCallFilter {
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub execution_id: Option<String>,
    pub workspace_id: Option<String>,
    pub key_id: Option<String>,
    pub limit: Option<i64>,
    /// Zero-based page offset used by bounded observability reads.
    #[serde(default)]
    pub offset: Option<i64>,
}
