use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BUNDLED_LITELLM_METADATA_SOURCE: &str = "litellm:bundled";
pub const LITELLM_METADATA_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const BUNDLED_LITELLM_METADATA_URL: &str =
    "bundled:crates/ai-fence-model-metadata/res/litellm_model_prices_and_context_window.json";

static BUNDLED_LITELLM_CATALOG: Lazy<Value> = Lazy::new(|| {
    serde_json::from_str(include_str!(
        "../res/litellm_model_prices_and_context_window.json"
    ))
    .expect("bundled LiteLLM model metadata catalog must be valid JSON")
});

pub fn bundled_litellm_catalog() -> &'static Value {
    &BUNDLED_LITELLM_CATALOG
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_parallel_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_system_messages: Option<bool>,
    /// Optional agent-specific recommendation. This is intentionally nested so
    /// future agents can add settings without overloading a generic `harness`
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ModelAgentMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Agent launch guidance associated with a model.
///
/// String fields deliberately remain open-ended: an older client can preserve
/// a newer agent or harness name instead of rejecting server metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelAgentMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_interpreter: Option<OpenInterpreterAgentMetadata>,
}

/// Open Interpreter-specific launch guidance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenInterpreterAgentMetadata {
    /// Open Interpreter harness name, for example `kimi-code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_guidance: Option<bool>,
}

/// Wire API selected for an Open Interpreter provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpenInterpreterWireApi {
    Responses,
    Chat,
    Messages,
}

impl OpenInterpreterWireApi {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::Messages => "messages",
        }
    }
}

/// A locally-derived Open Interpreter recommendation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInterpreterRecommendation {
    pub agent: String,
    /// `native` is represented as `Some("native")` to make the recommendation
    /// visible to setup. Config renderers must omit the TOML `harness` key for
    /// native mode because upstream treats the literal string as a custom
    /// harness.
    pub harness: Option<String>,
    pub harness_guidance: Option<bool>,
}

impl OpenInterpreterRecommendation {
    fn harness(agent: &str, harness: &str) -> Self {
        Self {
            agent: agent.to_string(),
            harness: Some(harness.to_string()),
            // This is upstream's default. Keeping it explicit makes a saved
            // preference stable if the upstream default changes later.
            harness_guidance: Some(true),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelPricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_million_input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_million_cached_input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_million_output_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelMetadataOverride {
    #[serde(default)]
    pub metadata: ModelMetadata,
    #[serde(default)]
    pub pricing: ModelPricing,
}

impl ModelMetadataOverride {
    pub fn overlay(&mut self, other: &ModelMetadataOverride) {
        self.metadata.overlay(&other.metadata);
        self.pricing.overlay(&other.pricing);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResolvedModelMetadata {
    pub model: String,
    #[serde(default)]
    pub metadata: ModelMetadata,
    #[serde(default)]
    pub pricing: ModelPricing,
    #[serde(default)]
    pub sources: Vec<String>,
}

impl ModelMetadata {
    pub fn merge_missing_from(&mut self, other: &ModelMetadata) {
        if self.display_name.is_none() {
            self.display_name = other.display_name.clone();
        }
        if self.provider.is_none() {
            self.provider = other.provider.clone();
        }
        if self.mode.is_none() {
            self.mode = other.mode.clone();
        }
        if self.max_input_tokens.is_none() {
            self.max_input_tokens = other.max_input_tokens;
        }
        if self.max_output_tokens.is_none() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if self.supports_tools.is_none() {
            self.supports_tools = other.supports_tools;
        }
        if self.supports_parallel_tools.is_none() {
            self.supports_parallel_tools = other.supports_parallel_tools;
        }
        if self.supports_reasoning.is_none() {
            self.supports_reasoning = other.supports_reasoning;
        }
        if self.supports_vision.is_none() {
            self.supports_vision = other.supports_vision;
        }
        if self.supports_system_messages.is_none() {
            self.supports_system_messages = other.supports_system_messages;
        }
        match (&mut self.agent, &other.agent) {
            (Some(current), Some(other)) => current.merge_missing_from(other),
            (None, Some(other)) => self.agent = Some(other.clone()),
            _ => {}
        }
        if self.source.is_none() {
            self.source = other.source.clone();
        }
    }

    pub fn overlay(&mut self, other: &ModelMetadata) {
        if other.display_name.is_some() {
            self.display_name = other.display_name.clone();
        }
        if other.provider.is_some() {
            self.provider = other.provider.clone();
        }
        if other.mode.is_some() {
            self.mode = other.mode.clone();
        }
        if other.max_input_tokens.is_some() {
            self.max_input_tokens = other.max_input_tokens;
        }
        if other.max_output_tokens.is_some() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if other.supports_tools.is_some() {
            self.supports_tools = other.supports_tools;
        }
        if other.supports_parallel_tools.is_some() {
            self.supports_parallel_tools = other.supports_parallel_tools;
        }
        if other.supports_reasoning.is_some() {
            self.supports_reasoning = other.supports_reasoning;
        }
        if other.supports_vision.is_some() {
            self.supports_vision = other.supports_vision;
        }
        if other.supports_system_messages.is_some() {
            self.supports_system_messages = other.supports_system_messages;
        }
        match (&mut self.agent, &other.agent) {
            (Some(current), Some(other)) => current.overlay(other),
            (_, Some(other)) => self.agent = Some(other.clone()),
            _ => {}
        }
        if other.source.is_some() {
            self.source = other.source.clone();
        }
    }
}

impl ModelAgentMetadata {
    pub fn merge_missing_from(&mut self, other: &ModelAgentMetadata) {
        if self.recommended_agent.is_none() {
            self.recommended_agent = other.recommended_agent.clone();
        }
        match (&mut self.open_interpreter, &other.open_interpreter) {
            (Some(current), Some(other)) => current.merge_missing_from(other),
            (None, Some(other)) => self.open_interpreter = Some(other.clone()),
            _ => {}
        }
    }

    pub fn overlay(&mut self, other: &ModelAgentMetadata) {
        if other.recommended_agent.is_some() {
            self.recommended_agent = other.recommended_agent.clone();
        }
        match (&mut self.open_interpreter, &other.open_interpreter) {
            (Some(current), Some(other)) => current.overlay(other),
            (_, Some(other)) => self.open_interpreter = Some(other.clone()),
            _ => {}
        }
    }
}

impl OpenInterpreterAgentMetadata {
    pub fn merge_missing_from(&mut self, other: &OpenInterpreterAgentMetadata) {
        if self.harness.is_none() {
            self.harness = other.harness.clone();
        }
        if self.harness_guidance.is_none() {
            self.harness_guidance = other.harness_guidance;
        }
    }

    pub fn overlay(&mut self, other: &OpenInterpreterAgentMetadata) {
        if other.harness.is_some() {
            self.harness = other.harness.clone();
        }
        if other.harness_guidance.is_some() {
            self.harness_guidance = other.harness_guidance;
        }
    }
}

impl ModelPricing {
    pub fn merge_missing_from(&mut self, other: &ModelPricing) {
        if self.cost_per_million_input_tokens.is_none() {
            self.cost_per_million_input_tokens = other.cost_per_million_input_tokens;
        }
        if self.cost_per_million_cached_input_tokens.is_none() {
            self.cost_per_million_cached_input_tokens = other.cost_per_million_cached_input_tokens;
        }
        if self.cost_per_million_output_tokens.is_none() {
            self.cost_per_million_output_tokens = other.cost_per_million_output_tokens;
        }
        if self.source.is_none() {
            self.source = other.source.clone();
        }
    }

    pub fn overlay(&mut self, other: &ModelPricing) {
        if other.cost_per_million_input_tokens.is_some() {
            self.cost_per_million_input_tokens = other.cost_per_million_input_tokens;
        }
        if other.cost_per_million_cached_input_tokens.is_some() {
            self.cost_per_million_cached_input_tokens = other.cost_per_million_cached_input_tokens;
        }
        if other.cost_per_million_output_tokens.is_some() {
            self.cost_per_million_output_tokens = other.cost_per_million_output_tokens;
        }
        if other.source.is_some() {
            self.source = other.source.clone();
        }
    }
}

/// Derive a conservative Open Interpreter recommendation from local model
/// metadata. This is intentionally available without backend authentication so
/// `ai-fence-cli setup` can make a useful proposal before OIDC login.
///
/// Explicit metadata overrides are handled by the caller; this function only
/// supplies the built-in fallback. Unknown providers deliberately return
/// `None` instead of silently changing an existing user's agent.
pub fn recommend_open_interpreter_harness(
    model: &str,
    provider: Option<&str>,
    wire_api: OpenInterpreterWireApi,
) -> Option<OpenInterpreterRecommendation> {
    let model = model.trim().to_ascii_lowercase();
    let provider = provider.unwrap_or_default().trim().to_ascii_lowercase();
    let contains = |needle: &str| model.contains(needle) || provider.contains(needle);

    // AI Fence model namespaces can contain `/anthropic/` to describe the
    // target protocol (for example `kimi/anthropic/k3`), so identify concrete
    // provider families before treating that segment as Claude.
    //
    // Kimi Code is Chat-only. In particular, `kimi/anthropic/k3` is an
    // Anthropic Messages route and must not inherit the harness used by
    // `kimi/completions/k3`.
    if contains("kimi") || contains("moonshot") {
        return match wire_api {
            OpenInterpreterWireApi::Chat => Some(OpenInterpreterRecommendation::harness(
                "interpreter",
                "kimi-code",
            )),
            // Claude Code is the conservative portable harness for both the
            // Anthropic Messages route and the Responses fallback. Native
            // mode is not available for Messages.
            OpenInterpreterWireApi::Responses | OpenInterpreterWireApi::Messages => Some(
                OpenInterpreterRecommendation::harness("claude", "claude-code"),
            ),
        };
    }
    // ZCode only has a native Open Interpreter route over Anthropic Messages.
    // Z.AI's common OpenAI-compatible endpoint is Chat, where `zcode` would be
    // misleading; leave it explicit there.
    if matches!(wire_api, OpenInterpreterWireApi::Messages)
        && (contains("zai") || contains("zhipu") || model.contains("glm"))
    {
        return Some(OpenInterpreterRecommendation::harness(
            "interpreter",
            "zcode",
        ));
    }
    // A custom namespace such as `madserver002/anthropic/model` describes an
    // Anthropic-compatible transport, not an actual Claude model. Claude
    // Code's native profile sends auxiliary requests for a hard-coded Claude
    // model (for example its title request), which cannot resolve through a
    // custom AI Fence target. ZCode keeps the configured model id intact
    // while using the same Messages wire protocol.
    if matches!(wire_api, OpenInterpreterWireApi::Messages) && is_custom_anthropic_namespace(&model)
    {
        return Some(OpenInterpreterRecommendation::harness(
            "interpreter",
            "zcode",
        ));
    }
    if contains("anthropic") || model.contains("claude") {
        return Some(OpenInterpreterRecommendation::harness(
            "claude",
            "claude-code",
        ));
    }
    if contains("qwen")
        || contains("dashscope")
        || model.starts_with("qwq")
        || model.contains("/qwq")
    {
        return Some(OpenInterpreterRecommendation::harness(
            "interpreter",
            "qwen-code",
        ));
    }
    if contains("deepseek") {
        return Some(OpenInterpreterRecommendation::harness(
            "interpreter",
            "claude-code-bare",
        ));
    }
    if contains("openai")
        || model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
    {
        return Some(OpenInterpreterRecommendation::harness("codex", "native"));
    }
    // An unknown Messages endpoint cannot run native. Claude Code is the
    // broadly compatible safe default, but only select it when the protocol
    // itself establishes the required transport.
    if matches!(wire_api, OpenInterpreterWireApi::Messages) {
        return Some(OpenInterpreterRecommendation::harness(
            "claude",
            "claude-code",
        ));
    }
    None
}

fn is_custom_anthropic_namespace(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let Some((namespace, remainder)) = model.split_once('/') else {
        return false;
    };
    remainder.starts_with("anthropic/") && !matches!(namespace, "anthropic" | "kimi" | "zai")
}

/// Resolve a model's explicit metadata recommendation, falling back to the
/// built-in local Open Interpreter heuristic.
pub fn resolve_agent_recommendation(
    model: &str,
    metadata: &ModelMetadata,
    wire_api: OpenInterpreterWireApi,
) -> Option<ModelAgentMetadata> {
    let fallback =
        recommend_open_interpreter_harness(model, metadata.provider.as_deref(), wire_api).map(
            |recommendation| ModelAgentMetadata {
                recommended_agent: Some(recommendation.agent),
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: recommendation.harness,
                    harness_guidance: recommendation.harness_guidance,
                }),
            },
        );

    if let Some(mut explicit) = metadata.agent.clone() {
        let incompatible_harness = explicit
            .open_interpreter
            .as_ref()
            .is_some_and(|interpreter| {
                !is_open_interpreter_harness_compatible(interpreter.harness.as_deref(), wire_api)
            });
        let model_preserving_fallback = fallback
            .as_ref()
            .and_then(|recommendation| recommendation.open_interpreter.as_ref())
            .and_then(|interpreter| interpreter.harness.as_deref())
            == Some("zcode")
            && explicit
                .open_interpreter
                .as_ref()
                .and_then(|interpreter| interpreter.harness.as_deref())
                .is_some_and(|harness| matches!(harness, "claude-code" | "claude-code-bare"))
            && is_custom_anthropic_namespace(model);
        if !incompatible_harness && !model_preserving_fallback {
            return Some(explicit);
        }

        // Server-side metadata is user-configurable and can outlive a route
        // migration. Never forward an incompatible harness to setup or the
        // dashboard. Prefer the route-aware built-in fallback (notably Kimi
        // Messages -> Claude Code); for a route with no fallback, retain only
        // the non-harness agent guidance.
        if fallback.is_some() {
            return fallback;
        }
        explicit.open_interpreter = None;
        return Some(explicit);
    }

    fallback
}

/// Whether a harness is supported by Open Interpreter for the configured wire
/// API. `None`, `""`, and `"native"` describe native mode; renderers must
/// omit the literal `native` configuration value (see
/// [`OpenInterpreterRecommendation`]).
pub fn is_open_interpreter_harness_compatible(
    harness: Option<&str>,
    wire_api: OpenInterpreterWireApi,
) -> bool {
    let harness = harness.unwrap_or("native").trim().to_ascii_lowercase();
    let harness = if harness.is_empty() {
        "native"
    } else {
        &harness
    };
    match wire_api {
        OpenInterpreterWireApi::Responses => {
            matches!(harness, "native" | "claude-code" | "claude-code-bare")
        }
        OpenInterpreterWireApi::Chat => !matches!(harness, "zcode"),
        OpenInterpreterWireApi::Messages => {
            matches!(harness, "claude-code" | "claude-code-bare" | "zcode")
        }
    }
}

/// Return a user-facing explanation for an incompatible explicit Open
/// Interpreter harness selection.
pub fn open_interpreter_harness_compatibility_error(
    harness: Option<&str>,
    wire_api: OpenInterpreterWireApi,
) -> Option<String> {
    if is_open_interpreter_harness_compatible(harness, wire_api) {
        return None;
    }
    let harness = harness.unwrap_or("native").trim();
    Some(match wire_api {
        OpenInterpreterWireApi::Messages => format!(
            "Open Interpreter harness '{}' is incompatible with wire_api = \"messages\"; use claude-code, claude-code-bare, or zcode",
            if harness.is_empty() { "native" } else { harness }
        ),
        OpenInterpreterWireApi::Responses => format!(
            "Open Interpreter harness '{}' is incompatible with wire_api = \"responses\"; use native, claude-code, or claude-code-bare",
            if harness.is_empty() { "native" } else { harness }
        ),
        OpenInterpreterWireApi::Chat => format!(
            "Open Interpreter harness '{}' is incompatible with wire_api = \"chat\"; zcode requires an Anthropic Messages endpoint",
            if harness.is_empty() { "native" } else { harness }
        ),
    })
}

pub fn resolve_builtin_metadata(model: &str) -> Option<ResolvedModelMetadata> {
    let normalized = model.trim();
    let kimi_model = normalized
        .strip_prefix("kimi/completions/")
        .or_else(|| normalized.strip_prefix("kimi/anthropic/"))
        .unwrap_or(normalized);
    let (display_name, established_coding_metadata) = match kimi_model {
        "kimi-for-coding" => ("K2.7 Coding", true),
        "kimi-for-coding-highspeed" => ("K2.7 Coding Highspeed", true),
        "k3-256k" => ("K3-256k", false),
        "k3" => ("K3", false),
        _ => return None,
    };
    Some(ResolvedModelMetadata {
        model: model.to_string(),
        metadata: ModelMetadata {
            display_name: Some(display_name.to_string()),
            provider: Some("kimi".to_string()),
            mode: Some("chat".to_string()),
            max_input_tokens: Some(262_144),
            max_output_tokens: established_coding_metadata.then_some(32_768),
            supports_tools: established_coding_metadata.then_some(true),
            supports_parallel_tools: established_coding_metadata.then_some(false),
            supports_reasoning: Some(true),
            supports_vision: Some(true),
            supports_system_messages: established_coding_metadata.then_some(true),
            agent: None,
            source: Some("builtin:kimi-model-catalog".to_string()),
        },
        pricing: ModelPricing::default(),
        sources: vec!["builtin:kimi-model-catalog".to_string()],
    })
}

pub fn resolve_from_litellm_catalog(model: &str, catalog: &Value) -> Option<ResolvedModelMetadata> {
    resolve_from_litellm_catalog_with_source(model, catalog, "litellm")
}

pub fn resolve_from_litellm_catalog_with_source(
    model: &str,
    catalog: &Value,
    source: &str,
) -> Option<ResolvedModelMetadata> {
    let obj = catalog.as_object()?;
    for key in litellm_lookup_keys(model) {
        if let Some(entry) = obj.get(&key) {
            if let Some(resolved) = litellm_entry_to_metadata(model, entry, source) {
                return Some(resolved);
            }
        }
    }
    None
}

pub fn litellm_lookup_keys(model: &str) -> Vec<String> {
    let mut keys = Vec::new();
    push_key(&mut keys, model);
    if let Some(rest) = model.strip_prefix("zai-anthropic/") {
        push_key(&mut keys, &format!("zai/{rest}"));
        push_key(&mut keys, rest);
    }
    if let Some(rest) = model.strip_prefix("zai/") {
        push_key(&mut keys, rest);
    }
    if let Some(rest) = model.strip_prefix("kimi/completions/") {
        push_key(&mut keys, rest);
        push_key(&mut keys, &format!("moonshot/{rest}"));
    }
    if let Some(rest) = model.strip_prefix("kimi/anthropic/") {
        push_key(&mut keys, rest);
        push_key(&mut keys, &format!("moonshot/{rest}"));
    }
    keys
}

fn push_key(keys: &mut Vec<String>, key: &str) {
    let key = key.trim();
    if !key.is_empty() && !keys.iter().any(|existing| existing == key) {
        keys.push(key.to_string());
    }
}

fn litellm_entry_to_metadata(
    model: &str,
    entry: &Value,
    source: &str,
) -> Option<ResolvedModelMetadata> {
    let obj = entry.as_object()?;
    let input_cost = obj
        .get("input_cost_per_token")
        .and_then(Value::as_f64)
        .map(|v| v * 1_000_000.0);
    let cached_cost = obj
        .get("cache_read_input_token_cost")
        .or_else(|| obj.get("cached_input_cost_per_token"))
        .and_then(Value::as_f64)
        .map(|v| v * 1_000_000.0);
    let output_cost = obj
        .get("output_cost_per_token")
        .and_then(Value::as_f64)
        .map(|v| v * 1_000_000.0);

    Some(ResolvedModelMetadata {
        model: model.to_string(),
        metadata: ModelMetadata {
            display_name: obj
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider: obj
                .get("litellm_provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            mode: obj.get("mode").and_then(Value::as_str).map(str::to_string),
            max_input_tokens: obj.get("max_input_tokens").and_then(Value::as_u64),
            max_output_tokens: obj.get("max_output_tokens").and_then(Value::as_u64),
            supports_tools: obj
                .get("supports_function_calling")
                .and_then(Value::as_bool),
            supports_parallel_tools: obj
                .get("supports_parallel_function_calling")
                .and_then(Value::as_bool),
            supports_reasoning: obj.get("supports_reasoning").and_then(Value::as_bool),
            supports_vision: obj.get("supports_vision").and_then(Value::as_bool),
            supports_system_messages: obj.get("supports_system_messages").and_then(Value::as_bool),
            agent: None,
            source: Some(source.to_string()),
        },
        pricing: ModelPricing {
            cost_per_million_input_tokens: input_cost,
            cost_per_million_cached_input_tokens: cached_cost,
            cost_per_million_output_tokens: output_cost,
            source: if input_cost.is_some() || output_cost.is_some() {
                Some(source.to_string())
            } else {
                None
            },
        },
        sources: vec![source.to_string()],
    })
}

pub fn merge_resolved_metadata(
    model: &str,
    override_value: Option<ModelMetadataOverride>,
    dynamic: Option<ResolvedModelMetadata>,
    builtin: Option<ResolvedModelMetadata>,
) -> ResolvedModelMetadata {
    let mut resolved = ResolvedModelMetadata {
        model: model.to_string(),
        ..Default::default()
    };

    if let Some(value) = builtin {
        resolved.metadata.merge_missing_from(&value.metadata);
        resolved.pricing.merge_missing_from(&value.pricing);
        resolved.sources.extend(value.sources);
    }
    if let Some(value) = dynamic {
        resolved.metadata.overlay(&value.metadata);
        resolved.pricing.overlay(&value.pricing);
        resolved.sources.extend(value.sources);
    }
    if let Some(value) = override_value {
        let metadata_source = value.metadata.source.clone();
        let pricing_source = value.pricing.source.clone();
        resolved.metadata.overlay(&value.metadata);
        resolved.pricing.overlay(&value.pricing);
        match (metadata_source, pricing_source) {
            (None, None) => resolved.sources.push("override".to_string()),
            (Some(metadata_source), None) => resolved.sources.push(metadata_source),
            (None, Some(pricing_source)) => resolved.sources.push(pricing_source),
            (Some(metadata_source), Some(pricing_source)) => {
                resolved.sources.push(metadata_source);
                resolved.sources.push(pricing_source);
            }
        }
    }
    resolved.sources.sort();
    resolved.sources.dedup();
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn normalize_line_endings(value: &str) -> String {
        value.replace("\r\n", "\n")
    }

    fn fetch_upstream_text(url: &str) -> String {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("build HTTP client")
            .get(url)
            .send()
            .expect("fetch upstream LiteLLM metadata snapshot")
            .error_for_status()
            .expect("fetch upstream LiteLLM metadata snapshot")
            .text()
            .expect("read upstream LiteLLM metadata snapshot")
    }

    #[test]
    fn litellm_metadata_resolves_zai_prefixed_model() {
        let catalog = json!({
            "zai/glm-4.6": {
                "litellm_provider": "zai",
                "mode": "chat",
                "max_input_tokens": 200000,
                "max_output_tokens": 128000,
                "input_cost_per_token": 0.0000006,
                "output_cost_per_token": 0.0000022,
                "supports_function_calling": true,
                "supports_reasoning": true
            }
        });

        let resolved =
            resolve_from_litellm_catalog("zai-anthropic/glm-4.6", &catalog).expect("metadata");

        assert_eq!(resolved.metadata.provider.as_deref(), Some("zai"));
        assert_eq!(resolved.metadata.max_input_tokens, Some(200000));
        assert_eq!(resolved.metadata.max_output_tokens, Some(128000));
        assert_eq!(resolved.pricing.cost_per_million_input_tokens, Some(0.6));
        assert_eq!(resolved.pricing.cost_per_million_output_tokens, Some(2.2));
    }

    #[test]
    fn builtin_metadata_covers_all_kimi_coding_models_and_namespaces() {
        for (model, display_name) in [
            ("kimi-for-coding", "K2.7 Coding"),
            ("kimi-for-coding-highspeed", "K2.7 Coding Highspeed"),
            ("k3-256k", "K3-256k"),
            ("k3", "K3"),
        ] {
            for alias in [
                model.to_string(),
                format!("kimi/completions/{model}"),
                format!("kimi/anthropic/{model}"),
            ] {
                let resolved = resolve_builtin_metadata(&alias).expect("metadata");
                assert_eq!(
                    resolved.metadata.display_name.as_deref(),
                    Some(display_name)
                );
                assert_eq!(resolved.metadata.max_input_tokens, Some(262_144));
                assert_eq!(resolved.metadata.supports_reasoning, Some(true));
                assert_eq!(resolved.metadata.supports_vision, Some(true));
                assert_eq!(resolved.pricing, ModelPricing::default());
            }
        }

        let k3 = resolve_builtin_metadata("kimi/completions/k3").expect("K3 metadata");
        assert_eq!(k3.metadata.max_output_tokens, None);
        assert_eq!(k3.metadata.supports_parallel_tools, None);
    }

    #[test]
    fn bundled_litellm_catalog_resolves_zai_metadata() {
        let resolved = resolve_from_litellm_catalog_with_source(
            "zai-anthropic/glm-4.6",
            bundled_litellm_catalog(),
            BUNDLED_LITELLM_METADATA_SOURCE,
        )
        .expect("bundled metadata");

        assert_eq!(resolved.metadata.provider.as_deref(), Some("zai"));
        assert!(resolved.metadata.max_input_tokens.is_some());
        assert_eq!(
            resolved.metadata.source.as_deref(),
            Some(BUNDLED_LITELLM_METADATA_SOURCE)
        );
        assert_eq!(
            resolved.pricing.source.as_deref(),
            Some(BUNDLED_LITELLM_METADATA_SOURCE)
        );
    }

    #[test]
    fn bundled_litellm_catalog_covers_current_codex_pricing() {
        let resolved = resolve_from_litellm_catalog_with_source(
            "gpt-5.6-sol",
            bundled_litellm_catalog(),
            BUNDLED_LITELLM_METADATA_SOURCE,
        )
        .expect("bundled gpt-5.6-sol metadata");

        assert_eq!(resolved.metadata.provider.as_deref(), Some("openai"));
        assert_eq!(resolved.metadata.max_input_tokens, Some(1_050_000));
        assert_eq!(resolved.pricing.cost_per_million_input_tokens, Some(5.0));
        assert_eq!(
            resolved.pricing.cost_per_million_cached_input_tokens,
            Some(0.5)
        );
        assert_eq!(resolved.pricing.cost_per_million_output_tokens, Some(30.0));
    }

    #[test]
    fn bundled_litellm_snapshot_matches_upstream() {
        let live_check_enabled = std::env::var("LITELLM_SNAPSHOT_LIVE_CHECK").as_deref() == Ok("1");
        if !live_check_enabled {
            eprintln!(
                "Skipping live LiteLLM snapshot drift check; set \
                 LITELLM_SNAPSHOT_LIVE_CHECK=1 to compare against upstream."
            );
            return;
        }

        let bundled = normalize_line_endings(include_str!(
            "../res/litellm_model_prices_and_context_window.json"
        ));
        let upstream = normalize_line_endings(&fetch_upstream_text(LITELLM_METADATA_URL));

        assert!(
            bundled == upstream,
            "Bundled LiteLLM metadata snapshot is stale. Update it with:\n\
             curl -L --fail {LITELLM_METADATA_URL} \
             -o crates/ai-fence-model-metadata/res/litellm_model_prices_and_context_window.json"
        );
    }

    #[test]
    fn metadata_override_overlays_field_by_field() {
        let mut base = ModelMetadataOverride {
            metadata: ModelMetadata {
                max_input_tokens: Some(100_000),
                supports_tools: Some(true),
                ..Default::default()
            },
            pricing: ModelPricing {
                cost_per_million_input_tokens: Some(0.5),
                cost_per_million_output_tokens: Some(1.5),
                ..Default::default()
            },
        };
        let dashboard = ModelMetadataOverride {
            metadata: ModelMetadata {
                max_output_tokens: Some(16_000),
                supports_tools: Some(false),
                ..Default::default()
            },
            pricing: ModelPricing {
                cost_per_million_output_tokens: Some(2.0),
                ..Default::default()
            },
        };

        base.overlay(&dashboard);

        assert_eq!(base.metadata.max_input_tokens, Some(100_000));
        assert_eq!(base.metadata.max_output_tokens, Some(16_000));
        assert_eq!(base.metadata.supports_tools, Some(false));
        assert_eq!(base.pricing.cost_per_million_input_tokens, Some(0.5));
        assert_eq!(base.pricing.cost_per_million_output_tokens, Some(2.0));
    }

    #[test]
    fn agent_metadata_merges_and_overlays_field_by_field() {
        let mut base = ModelMetadata {
            agent: Some(ModelAgentMetadata {
                recommended_agent: Some("interpreter".to_string()),
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: Some("kimi-code".to_string()),
                    harness_guidance: None,
                }),
            }),
            ..Default::default()
        };
        let overlay = ModelMetadata {
            agent: Some(ModelAgentMetadata {
                recommended_agent: None,
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: None,
                    harness_guidance: Some(false),
                }),
            }),
            ..Default::default()
        };

        base.overlay(&overlay);

        assert_eq!(
            base.agent
                .as_ref()
                .and_then(|agent| agent.recommended_agent.as_deref()),
            Some("interpreter")
        );
        let interpreter = base
            .agent
            .as_ref()
            .and_then(|agent| agent.open_interpreter.as_ref())
            .expect("interpreter metadata");
        assert_eq!(interpreter.harness.as_deref(), Some("kimi-code"));
        assert_eq!(interpreter.harness_guidance, Some(false));

        let mut missing = ModelMetadata::default();
        missing.merge_missing_from(&base);
        assert_eq!(missing.agent, base.agent);
    }

    #[test]
    fn agent_metadata_serde_is_nested_and_forward_compatible() {
        let metadata: ModelMetadata = serde_json::from_value(json!({
            "agent": {
                "recommended_agent": "interpreter",
                "open_interpreter": {
                    "harness": "kimi-code",
                    "harness_guidance": true,
                    "future_option": "ignored by this client"
                },
                "future_agent_option": true
            }
        }))
        .expect("deserialize recommendation");

        assert_eq!(
            metadata
                .agent
                .as_ref()
                .and_then(|agent| agent.open_interpreter.as_ref())
                .and_then(|interpreter| interpreter.harness.as_deref()),
            Some("kimi-code")
        );
        let serialized = serde_json::to_value(&metadata).expect("serialize recommendation");
        assert_eq!(serialized["agent"]["recommended_agent"], "interpreter");
        assert!(serialized["agent"].get("future_agent_option").is_none());
    }

    #[test]
    fn open_interpreter_recommendations_cover_supported_provider_families() {
        let cases = [
            (
                "gpt-5.6",
                Some("openai"),
                OpenInterpreterWireApi::Responses,
                "codex",
                "native",
            ),
            (
                "claude-sonnet-4",
                Some("anthropic"),
                OpenInterpreterWireApi::Messages,
                "claude",
                "claude-code",
            ),
            (
                "kimi/completions/k3",
                Some("moonshot"),
                OpenInterpreterWireApi::Chat,
                "interpreter",
                "kimi-code",
            ),
            (
                "qwen3-coder",
                Some("dashscope"),
                OpenInterpreterWireApi::Chat,
                "interpreter",
                "qwen-code",
            ),
            (
                "deepseek-v3.2",
                Some("deepseek"),
                OpenInterpreterWireApi::Chat,
                "interpreter",
                "claude-code-bare",
            ),
        ];

        for (model, provider, wire_api, expected_agent, expected_harness) in cases {
            let recommendation = recommend_open_interpreter_harness(model, provider, wire_api)
                .expect("recommendation");
            assert_eq!(recommendation.agent, expected_agent);
            assert_eq!(recommendation.harness.as_deref(), Some(expected_harness));
            assert_eq!(recommendation.harness_guidance, Some(true));
        }
    }

    #[test]
    fn kimi_messages_uses_claude_code_instead_of_chat_only_kimi_code() {
        let recommendation = recommend_open_interpreter_harness(
            "kimi/anthropic/k3",
            Some("kimi"),
            OpenInterpreterWireApi::Messages,
        )
        .expect("Kimi Messages recommendation");

        assert_eq!(recommendation.agent, "claude");
        assert_eq!(recommendation.harness.as_deref(), Some("claude-code"));
        assert!(is_open_interpreter_harness_compatible(
            recommendation.harness.as_deref(),
            OpenInterpreterWireApi::Messages,
        ));
    }

    #[test]
    fn glm_zcode_is_only_automatic_for_messages_routes() {
        assert_eq!(
            recommend_open_interpreter_harness(
                "zai-anthropic/glm-4.6",
                Some("zai"),
                OpenInterpreterWireApi::Messages,
            )
            .and_then(|recommendation| recommendation.harness),
            Some("zcode".to_string())
        );
        assert!(recommend_open_interpreter_harness(
            "zai/glm-4.6",
            Some("zai"),
            OpenInterpreterWireApi::Chat,
        )
        .is_none());
    }

    #[test]
    fn custom_anthropic_namespaces_use_model_preserving_messages_harness() {
        for model in ["madserver002/anthropic/model", "madgaming/anthropic/model"] {
            let recommendation =
                recommend_open_interpreter_harness(model, None, OpenInterpreterWireApi::Messages)
                    .expect("custom Messages recommendation");
            assert_eq!(recommendation.agent, "interpreter");
            assert_eq!(recommendation.harness.as_deref(), Some("zcode"));
        }
    }

    #[test]
    fn stale_claude_harness_is_replaced_for_custom_anthropic_namespace() {
        let metadata = ModelMetadata {
            provider: Some("madserver002".to_string()),
            agent: Some(ModelAgentMetadata {
                recommended_agent: Some("claude".to_string()),
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: Some("claude-code".to_string()),
                    harness_guidance: Some(true),
                }),
            }),
            ..Default::default()
        };

        let resolved = resolve_agent_recommendation(
            "madserver002/anthropic/model",
            &metadata,
            OpenInterpreterWireApi::Messages,
        )
        .expect("resolved metadata");
        assert_eq!(resolved.recommended_agent.as_deref(), Some("interpreter"));
        assert_eq!(
            resolved
                .open_interpreter
                .as_ref()
                .and_then(|interpreter| interpreter.harness.as_deref()),
            Some("zcode")
        );
    }

    #[test]
    fn explicit_incompatible_metadata_is_replaced_with_route_safe_guidance() {
        let metadata = ModelMetadata {
            provider: Some("kimi".to_string()),
            agent: Some(ModelAgentMetadata {
                recommended_agent: Some("interpreter".to_string()),
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: Some("kimi-code".to_string()),
                    harness_guidance: Some(true),
                }),
            }),
            ..Default::default()
        };

        let resolved = resolve_agent_recommendation(
            "kimi/anthropic/k3",
            &metadata,
            OpenInterpreterWireApi::Messages,
        )
        .expect("resolved metadata");

        assert_eq!(resolved.recommended_agent.as_deref(), Some("claude"));
        let interpreter = resolved
            .open_interpreter
            .expect("route-safe Open Interpreter metadata");
        assert_eq!(interpreter.harness.as_deref(), Some("claude-code"));
        assert!(is_open_interpreter_harness_compatible(
            interpreter.harness.as_deref(),
            OpenInterpreterWireApi::Messages,
        ));
    }

    #[test]
    fn explicit_incompatible_harness_is_removed_when_no_route_default_exists() {
        let metadata = ModelMetadata {
            provider: Some("zai".to_string()),
            agent: Some(ModelAgentMetadata {
                recommended_agent: Some("interpreter".to_string()),
                open_interpreter: Some(OpenInterpreterAgentMetadata {
                    harness: Some("zcode".to_string()),
                    harness_guidance: Some(true),
                }),
            }),
            ..Default::default()
        };

        let resolved =
            resolve_agent_recommendation("zai/glm-5", &metadata, OpenInterpreterWireApi::Chat)
                .expect("resolved metadata");

        assert_eq!(resolved.recommended_agent.as_deref(), Some("interpreter"));
        assert_eq!(resolved.open_interpreter, None);
    }

    #[test]
    fn open_interpreter_harness_protocol_matrix_is_conservative() {
        assert!(is_open_interpreter_harness_compatible(
            Some("native"),
            OpenInterpreterWireApi::Responses
        ));
        assert!(is_open_interpreter_harness_compatible(
            Some("kimi-code"),
            OpenInterpreterWireApi::Chat
        ));
        assert!(is_open_interpreter_harness_compatible(
            Some("zcode"),
            OpenInterpreterWireApi::Messages
        ));
        assert!(!is_open_interpreter_harness_compatible(
            Some("native"),
            OpenInterpreterWireApi::Messages
        ));
        assert!(!is_open_interpreter_harness_compatible(
            Some("kimi-code"),
            OpenInterpreterWireApi::Messages
        ));
        assert!(!is_open_interpreter_harness_compatible(
            Some("zcode"),
            OpenInterpreterWireApi::Chat
        ));
        assert!(open_interpreter_harness_compatibility_error(
            Some("native"),
            OpenInterpreterWireApi::Messages
        )
        .expect("error")
        .contains("claude-code"));
    }
}
