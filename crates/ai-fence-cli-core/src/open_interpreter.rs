//! Managed Open Interpreter configuration.
//!
//! Open Interpreter is derived from Codex, but it intentionally uses
//! `INTERPRETER_HOME` (defaulting to `~/.openinterpreter`) rather than
//! `CODEX_HOME`. Keep this module separate from Codex auth handling: AI Fence
//! must never seed an Interpreter run from Codex credentials or state.

use crate::agent_launcher::{
    write_ai_fence_model_catalog, AgentMcpServer, AgentProfile, CodexProviderAuth, ResolvedAgent,
};
use ai_fence_model_metadata::{
    is_open_interpreter_harness_compatible, open_interpreter_harness_compatibility_error,
    OpenInterpreterWireApi,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml::value::Table as TomlTable;

pub const INTERPRETER_HOME_ENV_VAR: &str = "INTERPRETER_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInterpreterModelConfig {
    pub model: String,
    pub harness: Option<String>,
    pub harness_guidance: Option<bool>,
    pub wire_api: OpenInterpreterWireApi,
}

#[derive(Debug, Clone)]
pub struct OpenInterpreterConfig<'a> {
    pub model: Option<&'a str>,
    /// Ordered models exposed through `/model`.
    pub selected_models: &'a [OpenInterpreterModelConfig],
    /// Default model used when Open Interpreter spawns a subagent.
    pub default_subagent_model: Option<&'a str>,
    pub harness: Option<&'a str>,
    pub harness_guidance: Option<bool>,
    pub wire_api: OpenInterpreterWireApi,
    pub yolo: bool,
    pub provider_auth: CodexProviderAuth,
    /// AI Fence home containing the durable `.openinterpreter/config.toml`
    /// template. This is intentionally separate from `.codex/config.toml`.
    pub template_dir: Option<&'a Path>,
    /// Managed Open Interpreter profile whose config overlays the global
    /// Interpreter template.
    pub profile: Option<&'a AgentProfile>,
}

/// The user-level home Open Interpreter itself resolves when it is not managed
/// by AI Fence. This intentionally does not inspect `CODEX_HOME`.
pub fn default_user_interpreter_home() -> Option<PathBuf> {
    std::env::var_os(INTERPRETER_HOME_ENV_VAR)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".openinterpreter"))
        })
}

/// Resolve a run-scoped Open Interpreter home and refuse the normal user home.
/// The caller must create the returned directory before launching so upstream
/// can canonicalize `INTERPRETER_HOME` successfully.
pub fn resolve_interpreter_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let interpreter_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".interpreter"));
    if let Some(default_home) = default_user_interpreter_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&interpreter_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default INTERPRETER_HOME {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(interpreter_home)
}

fn normalize_path_lexical(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

/// Write an isolated Open Interpreter `config.toml` for an AI Fence proxy.
///
/// This function does not read, copy, or hydrate Codex credentials. In
/// particular, `CodexProviderAuth::OpenAiAuth` is rejected because it would
/// make Interpreter start an independent OpenAI login flow rather than use the
/// proxy credentials supplied by AI Fence.
pub fn write_open_interpreter_config(
    interpreter_home: &Path,
    proxy_base: &str,
    options: OpenInterpreterConfig<'_>,
) -> Result<()> {
    let harness = normalize_harness(options.harness);
    if !is_open_interpreter_harness_compatible(harness, options.wire_api) {
        anyhow::bail!(
            "{}",
            open_interpreter_harness_compatibility_error(harness, options.wire_api)
                .expect("incompatible harness produces a diagnostic")
        );
    }
    if matches!(options.provider_auth, CodexProviderAuth::OpenAiAuth) {
        anyhow::bail!(
            "Open Interpreter managed runs require AI Fence proxy credentials; OpenAI subscription auth is not copied into INTERPRETER_HOME"
        );
    }

    fs::create_dir_all(interpreter_home).with_context(|| {
        format!(
            "failed to create INTERPRETER_HOME {}",
            interpreter_home.display()
        )
    })?;

    let mut config = read_open_interpreter_config_template(options.template_dir)?;
    if let Some(profile) = options.profile {
        merge_open_interpreter_config_overlay(
            &mut config,
            read_open_interpreter_profile_config(profile)?,
        );
    }
    if let Some(model) = options
        .model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        insert_string(&mut config, "model", model);
    }
    insert_string(&mut config, "model_provider", provider_id(options.wire_api));
    config.remove("harness");
    config.remove("harness_guidance");
    // `native` must be omitted: upstream parses the literal as an unknown
    // custom harness rather than as its native mode.
    if let Some(harness) = harness.filter(|harness| *harness != "native") {
        insert_string(&mut config, "harness", harness);
    }
    if let Some(enabled) = options.harness_guidance {
        config.insert(
            "harness_guidance".to_string(),
            toml::Value::Boolean(enabled),
        );
    }
    if options.yolo {
        // Verified against the current Open Interpreter config schema.
        insert_string(&mut config, "approval_policy", "never");
        insert_string(&mut config, "sandbox_mode", "danger-full-access");
    }

    for wire_api in [
        OpenInterpreterWireApi::Responses,
        OpenInterpreterWireApi::Chat,
        OpenInterpreterWireApi::Messages,
    ] {
        let provider = ensure_model_provider(&mut config, provider_id(wire_api));
        insert_string(provider, "name", provider_name(wire_api));
        let base_url = match wire_api {
            OpenInterpreterWireApi::Responses | OpenInterpreterWireApi::Chat => {
                format!("{proxy_base}/v1")
            }
            // The Messages transport appends `/v1/messages` itself.
            OpenInterpreterWireApi::Messages => proxy_base.trim_end_matches('/').to_string(),
        };
        insert_string(provider, "base_url", &base_url);
        insert_string(provider, "wire_api", wire_api.as_str());
        // AI Fence has an HTTPS fallback but WebSocket negotiation can add a
        // long failure delay. Managed configurations use HTTPS immediately.
        provider.insert(
            "supports_websockets".to_string(),
            toml::Value::Boolean(false),
        );
        apply_proxy_auth(provider, options.provider_auth.clone())?;
    }

    let mut selected_models = options
        .selected_models
        .iter()
        .map(|selected| selected.model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect::<Vec<_>>();
    if let Some(model) = options
        .model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        if !selected_models.iter().any(|selected| selected == model) {
            selected_models.insert(0, model.to_string());
        }
    }
    if !selected_models.is_empty() {
        let catalog_path = interpreter_home.join("model-catalog.json");
        write_ai_fence_model_catalog(&catalog_path, &selected_models)?;
        insert_string(
            &mut config,
            "model_catalog_json",
            &catalog_path.to_string_lossy(),
        );
    } else {
        config.remove("model_catalog_json");
    }
    write_interpreter_agent_roles(
        interpreter_home,
        &mut config,
        options.selected_models,
        options.default_subagent_model,
    )?;

    fs::write(
        interpreter_home.join("config.toml"),
        toml::to_string_pretty(&toml::Value::Table(config))
            .context("failed to render Open Interpreter TOML")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            interpreter_home.join("config.toml").display()
        )
    })
}

pub fn open_interpreter_profile_config_path(profile: &AgentProfile) -> PathBuf {
    profile.root_dir.join("config.toml")
}

pub fn upsert_open_interpreter_profile_mcp_server(
    profile: &AgentProfile,
    server: &AgentMcpServer,
) -> Result<()> {
    if profile.agent != ResolvedAgent::Interpreter {
        anyhow::bail!("Open Interpreter profile MCP config requires an Interpreter agent profile");
    }
    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let mut config = read_open_interpreter_profile_config(profile)?;
    let mcp_servers = ensure_table(&mut config, "mcp_servers");
    if server.name.trim().is_empty() {
        anyhow::bail!("MCP server name must not be empty");
    }
    mcp_servers.insert(
        server.name.clone(),
        toml::Value::Table(server.to_codex_toml()?),
    );
    let target = open_interpreter_profile_config_path(profile);
    fs::write(
        &target,
        toml::to_string_pretty(&toml::Value::Table(config))
            .context("failed to render Open Interpreter TOML")?,
    )
    .with_context(|| format!("failed to write {}", target.display()))
}

fn read_open_interpreter_config_template(template_dir: Option<&Path>) -> Result<TomlTable> {
    let Some(template_dir) = template_dir else {
        return Ok(TomlTable::new());
    };
    read_first_open_interpreter_config(&[
        template_dir.join(".openinterpreter").join("config.toml"),
        template_dir.join("openinterpreter-config.toml"),
    ])
}

fn read_open_interpreter_profile_config(profile: &AgentProfile) -> Result<TomlTable> {
    if profile.agent != ResolvedAgent::Interpreter {
        anyhow::bail!("Open Interpreter config requires an Interpreter agent profile");
    }
    read_first_open_interpreter_config(&[
        open_interpreter_profile_config_path(profile),
        profile
            .root_dir
            .join(".openinterpreter")
            .join("config.toml"),
        profile.root_dir.join("openinterpreter-config.toml"),
    ])
}

fn read_first_open_interpreter_config(paths: &[PathBuf]) -> Result<TomlTable> {
    let Some(path) = paths.iter().find(|path| path.is_file()) else {
        return Ok(TomlTable::new());
    };
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value = data
        .parse::<toml::Value>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    value.as_table().cloned().with_context(|| {
        format!(
            "Open Interpreter config {} must be a TOML table",
            path.display()
        )
    })
}

fn merge_open_interpreter_config_overlay(config: &mut TomlTable, overlay: TomlTable) {
    for (key, value) in overlay {
        if key == "mcp_servers" {
            merge_named_tables_replace_entries(config, key, value);
        } else {
            merge_toml_value(config, key, value);
        }
    }
}

fn merge_toml_value(parent: &mut TomlTable, key: String, overlay: toml::Value) {
    match (parent.get_mut(&key), overlay) {
        (Some(toml::Value::Table(base)), toml::Value::Table(overlay)) => {
            for (child_key, child_value) in overlay {
                merge_toml_value(base, child_key, child_value);
            }
        }
        (_, overlay) => {
            parent.insert(key, overlay);
        }
    }
}

fn merge_named_tables_replace_entries(
    parent: &mut TomlTable,
    key: impl Into<String>,
    overlay: toml::Value,
) {
    let key = key.into();
    let toml::Value::Table(overlay) = overlay else {
        parent.insert(key, overlay);
        return;
    };
    let base = ensure_table(parent, &key);
    for (name, value) in overlay {
        base.insert(name, value);
    }
}

fn normalize_harness(harness: Option<&str>) -> Option<&str> {
    harness
        .map(str::trim)
        .filter(|harness| !harness.is_empty())
        .map(|harness| {
            if harness.eq_ignore_ascii_case("native") {
                "native"
            } else {
                harness
            }
        })
}

fn apply_proxy_auth(provider: &mut TomlTable, auth: CodexProviderAuth) -> Result<()> {
    provider.remove("env_key");
    provider.remove("requires_openai_auth");
    provider.remove("auth");
    match auth {
        CodexProviderAuth::EnvKey => {
            insert_string(provider, "env_key", "OPENAI_API_KEY");
        }
        CodexProviderAuth::EnvBearer { env_key } => {
            insert_string(provider, "env_key", &env_key);
        }
        CodexProviderAuth::Command { command, args } => {
            let mut auth = TomlTable::new();
            insert_string(&mut auth, "command", &command);
            if !args.is_empty() {
                auth.insert(
                    "args".to_string(),
                    toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
                );
            }
            provider.insert("auth".to_string(), toml::Value::Table(auth));
        }
        CodexProviderAuth::OpenAiAuth => unreachable!("validated before rendering"),
    }
    Ok(())
}

fn provider_id(wire_api: OpenInterpreterWireApi) -> &'static str {
    match wire_api {
        OpenInterpreterWireApi::Responses => "ai_fence_responses",
        OpenInterpreterWireApi::Chat => "ai_fence_chat",
        OpenInterpreterWireApi::Messages => "ai_fence_messages",
    }
}

fn provider_name(wire_api: OpenInterpreterWireApi) -> &'static str {
    match wire_api {
        OpenInterpreterWireApi::Responses => "AI Fence - Responses",
        OpenInterpreterWireApi::Chat => "AI Fence - Chat",
        OpenInterpreterWireApi::Messages => "AI Fence - Messages",
    }
}

fn ensure_model_provider<'a>(config: &'a mut TomlTable, id: &str) -> &'a mut TomlTable {
    let providers = ensure_table(config, "model_providers");
    let provider = providers
        .entry(id.to_string())
        .or_insert_with(|| toml::Value::Table(TomlTable::new()));
    if !provider.is_table() {
        *provider = toml::Value::Table(TomlTable::new());
    }
    provider.as_table_mut().expect("table set above")
}

fn ensure_table<'a>(parent: &'a mut TomlTable, key: &str) -> &'a mut TomlTable {
    let value = parent
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Table(TomlTable::new()));
    if !value.is_table() {
        *value = toml::Value::Table(TomlTable::new());
    }
    value.as_table_mut().expect("table set above")
}

fn insert_string(table: &mut TomlTable, key: &str, value: &str) {
    table.insert(key.to_string(), toml::Value::String(value.to_string()));
}

fn write_interpreter_agent_roles(
    interpreter_home: &Path,
    config: &mut TomlTable,
    selected_models: &[OpenInterpreterModelConfig],
    default_subagent_model: Option<&str>,
) -> Result<()> {
    if selected_models.is_empty() {
        return Ok(());
    }
    let role_dir = interpreter_home.join("agents");
    fs::create_dir_all(&role_dir)
        .with_context(|| format!("failed to create {}", role_dir.display()))?;
    let mut role_models = selected_models
        .iter()
        .filter(|model| !model.model.trim().is_empty())
        .collect::<Vec<_>>();
    if let Some(default_model) = default_subagent_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        if let Some(index) = role_models
            .iter()
            .position(|model| model.model == default_model)
        {
            role_models.remove(index);
        }
        if let Some(default_model) = selected_models
            .iter()
            .find(|model| model.model == default_model)
        {
            role_models.insert(0, default_model);
        }
    }

    for (index, selected) in role_models.into_iter().enumerate() {
        let model = selected.model.trim();
        let harness = normalize_harness(selected.harness.as_deref());
        if !is_open_interpreter_harness_compatible(harness, selected.wire_api) {
            anyhow::bail!(
                "selected Open Interpreter model '{model}' is invalid: {}",
                open_interpreter_harness_compatibility_error(harness, selected.wire_api)
                    .expect("incompatible harness produces a diagnostic")
            );
        }
        let role_name = if index == 0 {
            "ai-fence-default".to_string()
        } else {
            format!("ai-fence-model-{:02}", index + 1)
        };
        let role_path = role_dir.join(format!("{role_name}.toml"));
        let mut role_config = TomlTable::new();
        insert_string(&mut role_config, "model", model);
        insert_string(
            &mut role_config,
            "model_provider",
            provider_id(selected.wire_api),
        );
        if let Some(harness) = harness.filter(|harness| *harness != "native") {
            insert_string(&mut role_config, "harness", harness);
        }
        if let Some(enabled) = selected.harness_guidance {
            role_config.insert(
                "harness_guidance".to_string(),
                toml::Value::Boolean(enabled),
            );
        }
        fs::write(
            &role_path,
            toml::to_string_pretty(&toml::Value::Table(role_config))
                .context("failed to render Open Interpreter agent role")?,
        )
        .with_context(|| format!("failed to write {}", role_path.display()))?;

        let agents = ensure_table(config, "agents");
        let role = ensure_table(agents, &role_name);
        insert_string(
            role,
            "description",
            &format!("Use AI Fence model {model} with its compatible harness."),
        );
        insert_string(role, "config_file", &role_path.to_string_lossy());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_interpreter_home_is_run_scoped_and_not_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let home =
            resolve_interpreter_home(&run_dir, None, ".ai-fence/runs").expect("interpreter home");

        assert_eq!(home, run_dir.join(".interpreter"));
    }

    #[test]
    fn renderer_uses_proxy_credentials_without_codex_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("interpreter");
        write_open_interpreter_config(
            &home,
            "http://127.0.0.1:42317",
            OpenInterpreterConfig {
                model: Some("kimi/completions/k3"),
                selected_models: &[
                    OpenInterpreterModelConfig {
                        model: "kimi/completions/k3".to_string(),
                        harness: Some("kimi-code".to_string()),
                        harness_guidance: Some(true),
                        wire_api: OpenInterpreterWireApi::Chat,
                    },
                    OpenInterpreterModelConfig {
                        model: "zai-anthropic/glm-5".to_string(),
                        harness: Some("zcode".to_string()),
                        harness_guidance: Some(false),
                        wire_api: OpenInterpreterWireApi::Messages,
                    },
                ],
                default_subagent_model: Some("zai-anthropic/glm-5"),
                harness: Some("kimi-code"),
                harness_guidance: Some(true),
                wire_api: OpenInterpreterWireApi::Chat,
                yolo: true,
                provider_auth: CodexProviderAuth::EnvBearer {
                    env_key: "AI_FENCE_PROXY_TOKEN".to_string(),
                },
                template_dir: None,
                profile: None,
            },
        )
        .expect("write config");

        let config = fs::read_to_string(home.join("config.toml")).expect("read config");
        assert!(config.contains("model = \"kimi/completions/k3\""));
        assert!(config.contains("model_provider = \"ai_fence_chat\""));
        assert!(config.contains("harness = \"kimi-code\""));
        assert!(config.contains("harness_guidance = true"));
        assert!(config.contains("approval_policy = \"never\""));
        assert!(config.contains("sandbox_mode = \"danger-full-access\""));
        assert!(config.contains("[model_providers.ai_fence_responses]"));
        assert!(config.contains("[model_providers.ai_fence_chat]"));
        assert!(config.contains("[model_providers.ai_fence_messages]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:42317/v1\""));
        assert!(config.contains("base_url = \"http://127.0.0.1:42317\""));
        assert!(config.contains("wire_api = \"chat\""));
        assert!(config.contains("model_catalog_json = "));
        assert!(config.contains("[agents.ai-fence-default]"));
        let catalog =
            fs::read_to_string(home.join("model-catalog.json")).expect("read model catalog");
        assert!(catalog.contains("kimi/completions/k3"));
        assert!(catalog.contains("zai-anthropic/glm-5"));
        let default_agent =
            fs::read_to_string(home.join("agents/ai-fence-default.toml")).expect("agent role");
        assert!(default_agent.contains("model = \"zai-anthropic/glm-5\""));
        assert!(default_agent.contains("model_provider = \"ai_fence_messages\""));
        assert!(default_agent.contains("harness = \"zcode\""));
        assert!(default_agent.contains("harness_guidance = false"));
        assert!(config.contains("env_key = \"AI_FENCE_PROXY_TOKEN\""));
        assert!(config.contains("supports_websockets = false"));
        assert!(!home.join("auth.json").exists());
        assert!(!config.contains("requires_openai_auth"));
    }

    #[test]
    fn renderer_merges_global_and_profile_mcp_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".openinterpreter");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("config.toml"),
            r#"
custom_setting = "global"

[mcp_servers.global_docs]
url = "https://global.example/mcp"

[mcp_servers.shared]
command = "node"
args = ["global.js"]
"#,
        )
        .expect("global config");

        let profile = crate::agent_launcher::resolve_agent_profile(
            &config_dir,
            "research",
            ResolvedAgent::Interpreter,
        )
        .expect("profile");
        fs::create_dir_all(&profile.root_dir).expect("profile dir");
        fs::write(
            open_interpreter_profile_config_path(&profile),
            r#"
custom_setting = "profile"

[mcp_servers.profile_search]
url = "https://profile.example/mcp"

[mcp_servers.shared]
command = "python"
args = ["profile.py"]
"#,
        )
        .expect("profile config");
        upsert_open_interpreter_profile_mcp_server(
            &profile,
            &AgentMcpServer::stdio("local_tools", "node")
                .with_arg("server.js")
                .with_env("TOKEN", "env:TOKEN"),
        )
        .expect("profile mcp");

        let home = temp.path().join("run").join(".interpreter");
        write_open_interpreter_config(
            &home,
            "http://127.0.0.1:42317",
            OpenInterpreterConfig {
                model: Some("kimi/completions/k3"),
                selected_models: &[],
                default_subagent_model: None,
                harness: Some("kimi-code"),
                harness_guidance: Some(true),
                wire_api: OpenInterpreterWireApi::Chat,
                yolo: false,
                provider_auth: CodexProviderAuth::EnvKey,
                template_dir: Some(&config_dir),
                profile: Some(&profile),
            },
        )
        .expect("write config");

        let config = fs::read_to_string(home.join("config.toml")).expect("config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        assert_eq!(parsed["custom_setting"].as_str(), Some("profile"));
        assert_eq!(
            parsed["mcp_servers"]["global_docs"]["url"].as_str(),
            Some("https://global.example/mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["profile_search"]["url"].as_str(),
            Some("https://profile.example/mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["shared"]["command"].as_str(),
            Some("python")
        );
        assert_eq!(
            parsed["mcp_servers"]["local_tools"]["env"]["TOKEN"].as_str(),
            Some("env:TOKEN")
        );
        assert_eq!(parsed["model_provider"].as_str(), Some("ai_fence_chat"));
        assert_eq!(
            parsed["model_providers"]["ai_fence_chat"]["base_url"].as_str(),
            Some("http://127.0.0.1:42317/v1")
        );
    }

    #[test]
    fn renderer_omits_native_harness_and_rejects_invalid_protocol_pair() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("interpreter");
        write_open_interpreter_config(
            &home,
            "http://127.0.0.1:42317",
            OpenInterpreterConfig {
                model: Some("openai/gpt-5.6-sol"),
                selected_models: &[],
                default_subagent_model: None,
                harness: Some("native"),
                harness_guidance: None,
                wire_api: OpenInterpreterWireApi::Responses,
                yolo: false,
                provider_auth: CodexProviderAuth::EnvKey,
                template_dir: None,
                profile: None,
            },
        )
        .expect("write config");
        let config = fs::read_to_string(home.join("config.toml")).expect("read config");
        assert!(!config.contains("harness ="));

        let err = write_open_interpreter_config(
            &temp.path().join("invalid"),
            "http://127.0.0.1:42317",
            OpenInterpreterConfig {
                model: Some("kimi/anthropic/k3"),
                selected_models: &[],
                default_subagent_model: None,
                harness: Some("kimi-code"),
                harness_guidance: None,
                wire_api: OpenInterpreterWireApi::Messages,
                yolo: false,
                provider_auth: CodexProviderAuth::EnvKey,
                template_dir: None,
                profile: None,
            },
        )
        .expect_err("messages cannot use kimi-code");
        assert!(err.to_string().contains("claude-code"));
    }

    #[test]
    fn renderer_rejects_codex_subscription_auth() {
        let temp = tempfile::tempdir().expect("tempdir");
        let err = write_open_interpreter_config(
            temp.path(),
            "http://127.0.0.1:42317",
            OpenInterpreterConfig {
                model: Some("openai/gpt-5.6-sol"),
                selected_models: &[],
                default_subagent_model: None,
                harness: None,
                harness_guidance: None,
                wire_api: OpenInterpreterWireApi::Responses,
                yolo: false,
                provider_auth: CodexProviderAuth::OpenAiAuth,
                template_dir: None,
                profile: None,
            },
        )
        .expect_err("subscription auth must not leak");
        assert!(err.to_string().contains("not copied"));
    }
}
