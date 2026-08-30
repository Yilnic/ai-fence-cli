//! Persisted CLI defaults for local setup.

use ai_fence_contract::PlaceholderStyle;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliConfig {
    pub fence_url: Option<String>,
    pub oidc_issuer: Option<String>,
    pub oidc_client_id: Option<String>,
    pub proxy_port: Option<u16>,
    pub default_run_mode: Option<CliRunMode>,
    pub default_auth_pool: Option<String>,
    pub standalone_endpoint: Option<String>,
    pub standalone_protocol: Option<String>,
    pub codex_standalone_auth: Option<CliCodexStandaloneAuth>,
    pub redaction_placeholder_style: Option<PlaceholderStyle>,
    pub redaction_guidance_note_enabled: Option<bool>,
}

pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("Could not determine config directory")?
        .join(config_dir_name());
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    Ok(dir)
}

fn config_dir_name() -> String {
    let channel = std::env::var("AI_FENCE_CLI_CHANNEL")
        .ok()
        .or_else(|| std::env::var("AI_FENCE_STAGE").ok());
    config_dir_name_for_channel(channel.as_deref())
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

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("cli.toml"))
}

pub fn load_config() -> Result<CliConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(CliConfig::default());
    }
    let contents = std::fs::read_to_string(&path)
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
    if update.fence_url.is_some() {
        current.fence_url = update.fence_url;
    }
    if update.oidc_issuer.is_some() {
        current.oidc_issuer = update.oidc_issuer;
    }
    if update.oidc_client_id.is_some() {
        current.oidc_client_id = update.oidc_client_id;
    }
    if update.proxy_port.is_some() {
        current.proxy_port = update.proxy_port;
    }
    if update.default_run_mode.is_some() {
        current.default_run_mode = update.default_run_mode;
    }
    if update.default_auth_pool.is_some() {
        current.default_auth_pool = update.default_auth_pool;
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
    save_config(&current)?;
    Ok(current)
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
}
