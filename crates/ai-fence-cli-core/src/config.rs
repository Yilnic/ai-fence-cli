//! Persisted CLI defaults for local setup.

use ai_fence_contract::PlaceholderStyle;
use ai_fence_model_metadata::OpenInterpreterWireApi;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CliRunMode {
    Auto,
    Backend,
    Standalone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CliCodexStandaloneAuth {
    Subscription,
    Api,
}

/// Backend credential source selected by setup. `None` on `CliConfig` means
/// automatic selection for compatibility with configurations written before
/// this setting existed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CliBackendAuthPreference {
    Auto,
    Oidc,
    ApiKey,
}

/// User-selected default agent. The enum only covers installed AI Fence
/// launchers; per-model config keeps a string harness for forward-compatible
/// Open Interpreter additions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CliDefaultAgent {
    Auto,
    Codex,
    Claude,
    Interpreter,
    Junie,
    Pi,
    Dsh,
    Kimi,
    Copilot,
}

impl CliDefaultAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Interpreter => "interpreter",
            Self::Junie => "junie",
            Self::Pi => "pi",
            Self::Dsh => "dsh",
            Self::Kimi => "kimi",
            Self::Copilot => "copilot",
        }
    }
}

/// The credential lane intentionally selected for a named launch profile.
/// This contains references only; credentials remain in the credential store.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CliLaunchAuthLane {
    Direct,
    LocalSubscription,
    AuthPool,
}

/// Normalize a configured auth-pool reference. `none` is the explicit
/// sentinel used by CLI setup and wrappers to disable an inherited pool.
pub fn normalize_auth_pool_reference(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case("none")).then(|| value.to_string())
}

/// Reusable, channel-local launch defaults. The map key is also the default
/// native agent state profile selected by `run --profile`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliLaunchProfile {
    pub agent: CliDefaultAgent,
    pub mode: CliRunMode,
    pub auth_lane: CliLaunchAuthLane,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_interpreter_harness: Option<String>,
    pub native_profile: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliModelAgentPreference {
    /// Explicit user override synchronized with the backend.
    pub agent: Option<CliDefaultAgent>,
    /// Explicit user override synchronized with the backend.
    pub open_interpreter_harness: Option<String>,
    /// Explicit user override synchronized with the backend.
    pub open_interpreter_harness_guidance: Option<bool>,
    /// Backend-derived recommendation cached locally for offline launches.
    pub recommended_agent: Option<CliDefaultAgent>,
    /// Backend-derived recommendation cached locally for offline launches.
    pub recommended_open_interpreter_harness: Option<String>,
    /// Backend-derived recommendation cached locally for offline launches.
    pub recommended_open_interpreter_harness_guidance: Option<bool>,
    /// Configured backend transport cached locally for model-specific roles.
    pub open_interpreter_wire_api: Option<OpenInterpreterWireApi>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliConfig {
    pub fence_url: Option<String>,
    pub oidc_issuer: Option<String>,
    pub oidc_client_id: Option<String>,
    /// Explicitly selected backend credential source. Older configs omit this
    /// field and retain the historical automatic precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_auth_preference: Option<CliBackendAuthPreference>,
    pub proxy_port: Option<u16>,
    pub default_run_mode: Option<CliRunMode>,
    pub default_auth_pool: Option<String>,
    /// Default model for direct (non-subscription) agent runs.
    pub default_model: Option<String>,
    /// Ordered models exposed to managed agent model pickers. The default
    /// model should also be present here when a direct-model setup is active.
    #[serde(default)]
    pub selected_models: Vec<String>,
    /// Default agent selected by setup. Auto preserves existing command-based
    /// behavior until an explicit per-model choice is saved.
    pub default_agent: Option<CliDefaultAgent>,
    /// Per-model local fallback preferences. Authenticated backend/user
    /// preferences are authoritative when available; this table supports setup
    /// before login and offline standalone use.
    #[serde(default)]
    pub model_agent_preferences: BTreeMap<String, CliModelAgentPreference>,
    /// Named launch profiles are deliberately local and channel-specific.
    /// They contain no API keys, OIDC tokens, or provider credentials.
    #[serde(default)]
    pub launch_profiles: BTreeMap<String, CliLaunchProfile>,
    pub standalone_endpoint: Option<String>,
    pub standalone_protocol: Option<String>,
    pub codex_standalone_auth: Option<CliCodexStandaloneAuth>,
    pub redaction_placeholder_style: Option<PlaceholderStyle>,
    pub redaction_guidance_note_enabled: Option<bool>,
}

pub fn config_dir() -> Result<PathBuf> {
    config_dir_for_channel(current_channel().as_deref())
}

/// Return the channel-local configuration directory without changing process
/// environment. Installers use this to avoid reading production profiles when
/// installing a staged channel.
pub fn config_dir_for_channel(channel: Option<&str>) -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("Could not determine config directory")?
        .join(config_dir_name_for_channel(channel));
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    Ok(dir)
}

fn current_channel() -> Option<String> {
    std::env::var("AI_FENCE_CLI_CHANNEL")
        .ok()
        .or_else(|| std::env::var("AI_FENCE_STAGE").ok())
}

fn config_dir_name_for_channel(channel: Option<&str>) -> String {
    let Some(channel) = channel
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty() && value != "prod")
    else {
        return "ai-fence".to_string();
    };
    let sanitized: String = channel
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("ai-fence-{sanitized}")
}

#[cfg(test)]
fn config_path_under(root: &std::path::Path, channel: Option<&str>) -> PathBuf {
    root.join(config_dir_name_for_channel(channel))
        .join("cli.toml")
}

pub fn config_path() -> Result<PathBuf> {
    config_path_for_channel(current_channel().as_deref())
}

/// Return the configuration path for an explicit install channel.
pub fn config_path_for_channel(channel: Option<&str>) -> Result<PathBuf> {
    Ok(config_dir_for_channel(channel)?.join("cli.toml"))
}

pub fn load_config() -> Result<CliConfig> {
    load_config_for_channel(current_channel().as_deref())
}

/// Load configuration for an explicit channel. This is deliberately separate
/// from the ambient environment so `install-self --channel dev` cannot pick up
/// production launch profiles.
pub fn load_config_for_channel(channel: Option<&str>) -> Result<CliConfig> {
    load_config_at_path(&config_path_for_channel(channel)?)
}

fn load_config_at_path(path: &std::path::Path) -> Result<CliConfig> {
    if !path.exists() {
        return Ok(CliConfig::default());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn save_config(config: &CliConfig) -> Result<()> {
    let path = config_path()?;
    let contents = toml::to_string_pretty(config)?;
    std::fs::write(&path, contents)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

pub fn merge_and_save(update: CliConfig) -> Result<CliConfig> {
    let mut current = load_config()?;
    merge_config(&mut current, update);
    save_config(&current)?;
    Ok(current)
}

/// Apply the non-empty fields from a setup update to an existing config.
///
/// This is separate from persistence so callers which need to derive a
/// profile-local view of setup choices can do so without changing global
/// launch defaults.
pub fn merge_config(current: &mut CliConfig, update: CliConfig) {
    if update.fence_url.is_some() {
        current.fence_url = update.fence_url;
    }
    if update.oidc_issuer.is_some() {
        current.oidc_issuer = update.oidc_issuer;
    }
    if update.oidc_client_id.is_some() {
        current.oidc_client_id = update.oidc_client_id;
    }
    if update.backend_auth_preference.is_some() {
        current.backend_auth_preference = update.backend_auth_preference;
    }
    if update.proxy_port.is_some() {
        current.proxy_port = update.proxy_port;
    }
    if update.default_run_mode.is_some() {
        current.default_run_mode = update.default_run_mode;
    }
    if let Some(auth_pool) = update.default_auth_pool {
        current.default_auth_pool = normalize_auth_pool_reference(&auth_pool);
    }
    if update.default_model.is_some() {
        current.default_model = update.default_model;
    }
    if !update.selected_models.is_empty() {
        current.selected_models = update.selected_models;
    }
    if update.default_agent.is_some() {
        current.default_agent = update.default_agent;
    }
    merge_model_agent_preferences(
        &mut current.model_agent_preferences,
        update.model_agent_preferences,
    );
    if !update.launch_profiles.is_empty() {
        current.launch_profiles.extend(update.launch_profiles);
    }
    if update.standalone_endpoint.is_some() {
        current.standalone_endpoint = update.standalone_endpoint;
    }
    if update.standalone_protocol.is_some() {
        current.standalone_protocol = update.standalone_protocol;
    }
    if update.codex_standalone_auth.is_some() {
        current.codex_standalone_auth = update.codex_standalone_auth;
    }
    if update.redaction_placeholder_style.is_some() {
        current.redaction_placeholder_style = update.redaction_placeholder_style;
    }
    if update.redaction_guidance_note_enabled.is_some() {
        current.redaction_guidance_note_enabled = update.redaction_guidance_note_enabled;
    }
}

fn merge_model_agent_preferences(
    current: &mut BTreeMap<String, CliModelAgentPreference>,
    updates: BTreeMap<String, CliModelAgentPreference>,
) {
    for (model, update) in updates {
        let current_preference = current.entry(model).or_default();
        if update.agent.is_some() {
            current_preference.agent = update.agent;
        }
        if update.open_interpreter_harness.is_some() {
            current_preference.open_interpreter_harness = update.open_interpreter_harness;
        }
        if update.open_interpreter_harness_guidance.is_some() {
            current_preference.open_interpreter_harness_guidance =
                update.open_interpreter_harness_guidance;
        }
        if update.recommended_agent.is_some() {
            current_preference.recommended_agent = update.recommended_agent;
        }
        if update.recommended_open_interpreter_harness.is_some() {
            current_preference.recommended_open_interpreter_harness =
                update.recommended_open_interpreter_harness;
        }
        if update
            .recommended_open_interpreter_harness_guidance
            .is_some()
        {
            current_preference.recommended_open_interpreter_harness_guidance =
                update.recommended_open_interpreter_harness_guidance;
        }
        if update.open_interpreter_wire_api.is_some() {
            current_preference.open_interpreter_wire_api = update.open_interpreter_wire_api;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_name_is_channel_specific() {
        assert_eq!(config_dir_name_for_channel(None), "ai-fence");
        assert_eq!(config_dir_name_for_channel(Some("prod")), "ai-fence");
        assert_eq!(config_dir_name_for_channel(Some("dev")), "ai-fence-dev");
        assert_eq!(
            config_dir_name_for_channel(Some("qa/west")),
            "ai-fence-qa_west"
        );
    }

    #[test]
    fn explicit_channel_paths_load_distinct_profiles_without_mutating_process_env() {
        let root = tempfile::tempdir().expect("tempdir");
        let production = config_path_under(root.path(), None);
        let development = config_path_under(root.path(), Some("dev"));
        assert_ne!(production, development);
        std::fs::create_dir_all(production.parent().expect("production parent"))
            .expect("create production config dir");
        std::fs::create_dir_all(development.parent().expect("development parent"))
            .expect("create development config dir");
        std::fs::write(&production, "default_model = \"openai/gpt-5.6-terra\"\n")
            .expect("write production config");
        std::fs::write(&development, "default_model = \"kimi/completions/k3\"\n")
            .expect("write development config");
        assert_eq!(
            load_config_at_path(&production)
                .expect("load production")
                .default_model
                .as_deref(),
            Some("openai/gpt-5.6-terra")
        );
        assert_eq!(
            load_config_at_path(&development)
                .expect("load development")
                .default_model
                .as_deref(),
            Some("kimi/completions/k3")
        );
    }

    #[test]
    fn default_model_round_trips_in_cli_config() {
        let config = CliConfig {
            default_model: Some("openai/gpt-5.6-sol".to_string()),
            backend_auth_preference: Some(CliBackendAuthPreference::Oidc),
            selected_models: vec![
                "openai/gpt-5.6-sol".to_string(),
                "kimi/completions/k3".to_string(),
            ],
            default_agent: Some(CliDefaultAgent::Interpreter),
            model_agent_preferences: BTreeMap::from([(
                "kimi/completions/k3".to_string(),
                CliModelAgentPreference {
                    agent: Some(CliDefaultAgent::Interpreter),
                    open_interpreter_harness: Some("kimi-code".to_string()),
                    open_interpreter_harness_guidance: Some(true),
                    recommended_agent: Some(CliDefaultAgent::Interpreter),
                    recommended_open_interpreter_harness: Some("kimi-code".to_string()),
                    recommended_open_interpreter_harness_guidance: Some(true),
                    open_interpreter_wire_api: Some(OpenInterpreterWireApi::Chat),
                },
            )]),
            ..Default::default()
        };

        let serialized = toml::to_string(&config).expect("serialize config");
        assert!(serialized.contains("default_model = \"openai/gpt-5.6-sol\""));
        let parsed: CliConfig = toml::from_str(&serialized).expect("parse config");
        assert_eq!(parsed.default_model, config.default_model);
        assert_eq!(
            parsed.backend_auth_preference,
            config.backend_auth_preference
        );
        assert_eq!(parsed.selected_models, config.selected_models);
        assert_eq!(parsed.default_agent, config.default_agent);
        assert_eq!(
            parsed.model_agent_preferences,
            config.model_agent_preferences
        );
    }

    #[test]
    fn model_preference_updates_merge_fields_without_losing_cached_values() {
        let model = "zai-anthropic/glm-5".to_string();
        let mut current = BTreeMap::from([(
            model.clone(),
            CliModelAgentPreference {
                agent: Some(CliDefaultAgent::Interpreter),
                open_interpreter_harness: Some("zcode".to_string()),
                open_interpreter_harness_guidance: Some(false),
                recommended_agent: Some(CliDefaultAgent::Interpreter),
                recommended_open_interpreter_harness: Some("zcode".to_string()),
                recommended_open_interpreter_harness_guidance: Some(true),
                open_interpreter_wire_api: Some(OpenInterpreterWireApi::Messages),
            },
        )]);
        merge_model_agent_preferences(
            &mut current,
            BTreeMap::from([(
                model.clone(),
                CliModelAgentPreference {
                    open_interpreter_harness: Some("claude-code-bare".to_string()),
                    ..Default::default()
                },
            )]),
        );

        let merged = current.get(&model).expect("merged preference");
        assert_eq!(merged.agent, Some(CliDefaultAgent::Interpreter));
        assert_eq!(
            merged.open_interpreter_harness.as_deref(),
            Some("claude-code-bare")
        );
        assert_eq!(merged.open_interpreter_harness_guidance, Some(false));
        assert_eq!(
            merged.open_interpreter_wire_api,
            Some(OpenInterpreterWireApi::Messages)
        );
    }

    #[test]
    fn launch_profiles_round_trip_and_old_config_stays_compatible() {
        let old: CliConfig =
            toml::from_str("default_model = \"kimi/completions/k3\"\n").expect("old config parses");
        assert!(old.launch_profiles.is_empty());

        let config = CliConfig {
            launch_profiles: BTreeMap::from([(
                "kimi".to_string(),
                CliLaunchProfile {
                    agent: CliDefaultAgent::Interpreter,
                    mode: CliRunMode::Backend,
                    auth_lane: CliLaunchAuthLane::Direct,
                    auth_pool: None,
                    model: Some("kimi/completions/k3".to_string()),
                    open_interpreter_harness: Some("kimi-code".to_string()),
                    native_profile: "kimi".to_string(),
                },
            )]),
            ..Default::default()
        };
        let serialized = toml::to_string_pretty(&config).expect("serialize config");
        assert!(serialized.contains("[launch_profiles.kimi]"));
        assert!(!serialized.contains("api_key"));
        assert_eq!(
            toml::from_str::<CliConfig>(&serialized)
                .expect("parse config")
                .launch_profiles,
            config.launch_profiles
        );
    }

    #[test]
    fn auth_pool_reference_normalizes_disable_sentinel_and_whitespace() {
        assert_eq!(
            normalize_auth_pool_reference(" first "),
            Some("first".into())
        );
        assert_eq!(normalize_auth_pool_reference("none"), None);
        assert_eq!(normalize_auth_pool_reference("NoNe"), None);
        assert_eq!(normalize_auth_pool_reference("   "), None);
    }

    #[test]
    fn old_configs_default_to_automatic_backend_auth() {
        let config: CliConfig =
            toml::from_str("fence_url = \"https://fence.example\"\n").expect("old config parses");
        assert_eq!(config.backend_auth_preference, None);

        let mut current = CliConfig::default();
        merge_config(
            &mut current,
            CliConfig {
                backend_auth_preference: Some(CliBackendAuthPreference::Oidc),
                ..Default::default()
            },
        );
        assert_eq!(
            current.backend_auth_preference,
            Some(CliBackendAuthPreference::Oidc)
        );
    }
}
