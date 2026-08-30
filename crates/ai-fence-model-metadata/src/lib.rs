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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
        if other.source.is_some() {
            self.source = other.source.clone();
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

pub fn resolve_builtin_metadata(model: &str) -> Option<ResolvedModelMetadata> {
    let normalized = model.trim();
    if matches!(
        normalized,
        "kimi-for-coding" | "kimi/completions/kimi-for-coding" | "kimi/anthropic/kimi-for-coding"
    ) {
        return Some(ResolvedModelMetadata {
            model: model.to_string(),
            metadata: ModelMetadata {
                display_name: Some("K2.7 Code".to_string()),
                provider: Some("kimi".to_string()),
                mode: Some("chat".to_string()),
                max_input_tokens: Some(262_144),
                max_output_tokens: Some(32_768),
                supports_tools: Some(true),
                supports_parallel_tools: Some(false),
                supports_reasoning: Some(true),
                supports_vision: Some(true),
                supports_system_messages: Some(true),
                source: Some("builtin:kimi-coding-docs".to_string()),
            },
            pricing: ModelPricing::default(),
            sources: vec!["builtin:kimi-coding-docs".to_string()],
        });
    }
    None
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
    fn builtin_metadata_covers_kimi_for_coding() {
        let resolved =
            resolve_builtin_metadata("kimi/anthropic/kimi-for-coding").expect("metadata");

        assert_eq!(resolved.metadata.max_input_tokens, Some(262144));
        assert_eq!(resolved.metadata.supports_reasoning, Some(true));
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
}
