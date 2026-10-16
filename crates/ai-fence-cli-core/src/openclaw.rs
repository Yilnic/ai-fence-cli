//! OpenClaw's native custom-provider config and isolated local state.
//!
//! Schema verified against the official OpenClaw v2026.9.8 release. Transport
//! selection belongs to the caller; this adapter never converts requests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::agent_launcher::{effective_user_home_dir, normalize_path_lexical, PiProviderConfig};

pub const PROVIDER_NAME: &str = "ai-fence";
pub const API_KEY_ENV_VAR: &str = "AI_FENCE_OPENCLAW_API_KEY";
pub const STATE_DIR_ENV_VAR: &str = "OPENCLAW_STATE_DIR";
pub const CONFIG_PATH_ENV_VAR: &str = "OPENCLAW_CONFIG_PATH";

pub fn model_ref(model: &str) -> String {
    format!("{PROVIDER_NAME}/{}", model_route(model))
}

pub fn model_route(model: &str) -> &str {
    model.strip_prefix("ai-fence/").unwrap_or(model)
}

pub fn validate_args(args: &[&str]) -> Result<()> {
    for arg in args.iter().take_while(|arg| **arg != "--") {
        let flag = arg.split('=').next().unwrap_or(arg);
        if matches!(
            flag,
            "--profile"
                | "--dev"
                | "--state-dir"
                | "--config"
                | "--isolated"
                | "--auth-env-only"
                | "--fallback"
                | "--container"
        ) {
            anyhow::bail!(
                "{flag} overrides managed OpenClaw state or routing; use AI Fence --profile, --openclaw-home, and --model"
            );
        }
    }
    Ok(())
}

/// Refuse personal/default OpenClaw state, including symlink aliases and
/// inherited overrides. The managed root is otherwise always run-scoped.
pub fn resolve_openclaw_home(run_dir: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    let requested = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| run_dir.join(".openclaw"));
    let mut personal = Vec::new();
    if let Some(home) = effective_user_home_dir() {
        personal.push(home.join(".openclaw"));
        personal.push(home.join(".clawdbot"));
    }
    if let Some(home) = std::env::var_os("OPENCLAW_HOME") {
        personal.push(PathBuf::from(home).join(".openclaw"));
    }
    if let Some(state_dir) = std::env::var_os(STATE_DIR_ENV_VAR) {
        personal.push(PathBuf::from(state_dir));
    }
    let requested = normalized_real_path(&requested)?;
    for path in personal {
        let path = normalized_real_path(&path)?;
        if requested.starts_with(&path) {
            anyhow::bail!(
                "refusing to use personal OpenClaw state {}; use a managed --openclaw-home directory",
                path.display()
            );
        }
    }
    if let Some(home) = effective_user_home_dir() {
        let home = normalized_real_path(&home)?;
        if requested.parent() == Some(home.as_path())
            && requested
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".openclaw-"))
        {
            anyhow::bail!("refusing to use a personal OpenClaw named profile");
        }
    }
    Ok(requested)
}

fn normalized_real_path(path: &Path) -> Result<PathBuf> {
    let path = normalize_path_lexical(path)?;
    // Resolve existing ancestors as well as the leaf: an uncreated directory
    // below a symlink must not bypass the personal-state check.
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    while fs::symlink_metadata(ancestor).is_err() {
        if let Some(name) = ancestor.file_name() {
            suffix.push(name.to_os_string());
        }
        ancestor = ancestor.parent().context("path has no existing ancestor")?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Pin one Fence route, with no built-in model discovery or fallback. The
/// credential is an env SecretRef, so generated catalogs retain a source
/// marker instead of persisting a resolved token.
pub fn write_openclaw_config(
    state_dir: &Path,
    provider: &PiProviderConfig,
    model: &str,
    workspace: &Path,
) -> Result<PathBuf> {
    let config = render_openclaw_config(state_dir, provider, model, workspace)?;
    fs::create_dir_all(state_dir.join("agents/main/agent"))?;
    fs::create_dir_all(state_dir.join("sessions"))?;
    fs::create_dir_all(state_dir.join("home"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = state_dir.join("openclaw.json");
    fs::write(&path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Render without touching the filesystem so trusted service launchers can
/// reuse the same native routing and SecretRef contract as local CLI runs.
pub fn render_openclaw_config(
    state_dir: &Path,
    provider: &PiProviderConfig,
    model: &str,
    workspace: &Path,
) -> Result<serde_json::Value> {
    let model = model_route(model).trim();
    if model.is_empty() || !provider.models.iter().any(|id| id == model) {
        anyhow::bail!("OpenClaw requires an explicit model in the managed Fence provider");
    }
    let qualified_model = model_ref(model);
    Ok(serde_json::json!({
        "models": {
            "mode": "replace",
            "catalogRefresh": { "enabled": false },
            "providers": {
                (PROVIDER_NAME): {
                    "baseUrl": provider.base_url,
                    "api": provider.api.as_str(),
                    "auth": "api-key",
                    "authHeader": true,
                    "apiKey": { "source": "env", "provider": "default", "id": API_KEY_ENV_VAR },
                    "models": [{ "id": model, "name": model }]
                }
            }
        },
        "agents": {
            "defaults": {
                "model": { "primary": qualified_model, "fallbacks": [] },
                "modelPolicy": { "allow": [qualified_model] },
                "workspace": workspace,
                "skipBootstrap": true
            },
            "entries": {
                "main": {
                    "agentDir": state_dir.join("agents/main/agent"),
                    "runtime": { "type": "embedded" }
                }
            }
        },
        "session": { "store": state_dir.join("sessions/sessions.json") },
        "gateway": { "mode": "local", "bind": "loopback" },
        "logging": { "file": state_dir.join("openclaw.log") },
        "env": { "shellEnv": { "enabled": false } }
    }))
}

/// An authenticated loopback Gateway exposing external client functions.
/// Native tools, plugins, background inference, and remote context fetching are
/// disabled; the embedding application retains tool authorization/execution.
pub fn render_gateway_config(
    state_dir: &Path,
    provider: &PiProviderConfig,
    model: &str,
    workspace: &Path,
    port: u16,
) -> Result<serde_json::Value> {
    let broker = reqwest::Url::parse(&provider.base_url)?;
    if broker.scheme() != "https"
        || broker.host_str().is_none()
        || !broker.username().is_empty()
        || broker.password().is_some()
        || broker.query().is_some()
        || broker.fragment().is_some()
        || broker.path() != "/v1"
        || port == 0
    {
        anyhow::bail!("OpenClaw Gateway requires an approved HTTPS broker URL and a nonzero port");
    }
    let mut config = render_openclaw_config(state_dir, provider, model, workspace)?;
    config["gateway"] = serde_json::json!({
        "mode":"local", "bind":"loopback", "port":port,
        "auth":{"mode":"token", "token":"${OPENCLAW_GATEWAY_TOKEN}"},
        "controlUi":{"enabled":false}, "reload":{"mode":"off"},
        "http":{"endpoints":{"responses":{"enabled":true,
            "files":{"allowUrl":false}, "images":{"allowUrl":false}}}}
    });
    let defaults = &mut config["agents"]["defaults"];
    defaults["contextInjection"] = serde_json::json!("never");
    defaults["skills"] = serde_json::json!([]);
    defaults["timeoutSeconds"] = serde_json::json!(30);
    defaults["heartbeat"] = serde_json::json!({"every":"0m"});
    config["plugins"] = serde_json::json!({"enabled":false});
    config["tools"] = serde_json::json!({"profile":"minimal", "deny":["*"]});
    config["cron"] = serde_json::json!({"enabled":false});
    config["discovery"] = serde_json::json!({"mdns":{"mode":"off"}});
    config["update"] = serde_json::json!({"checkOnStart":false, "auto":{"enabled":false}});
    config["logging"] = serde_json::json!({"level":"info", "consoleStyle":"json",
        "file":state_dir.join("openclaw.log"), "maxFileBytes":1048576});
    config["diagnostics"] = serde_json::json!({"enabled":false});
    Ok(config)
}

/// OpenClaw's native plugins and subprocesses must not inherit subscription,
/// provider, or unrelated application credentials from the launcher. Keep
/// only ordinary process/terminal settings and the scoped Fence key.
pub fn configure_environment(command: &mut Command, state_dir: &Path, api_key: &str) {
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "COLORTERM",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "TMP",
        "TEMP",
        "NO_COLOR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env(API_KEY_ENV_VAR, api_key);
    command.env(STATE_DIR_ENV_VAR, state_dir);
    command.env(CONFIG_PATH_ENV_VAR, state_dir.join("openclaw.json"));
    command.env("OPENCLAW_HOME", state_dir.join("home"));
    command.env("OPENCLAW_AGENT_DIR", state_dir.join("agents/main/agent"));
    command.env("OPENCLAW_CONFIG_READONLY", "1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_launcher::{
        resolve_agent_profile, resolve_launch, sync_profile_state_to_runtime,
        sync_runtime_state_to_profile, AgentKind, PiProviderApi, ResolvedAgent,
    };

    #[test]
    fn openclaw_selection_and_detection_are_distinct_from_interpreter() {
        let launch = resolve_launch(AgentKind::OpenClaw, &[]).unwrap();
        assert_eq!(launch.agent, ResolvedAgent::OpenClaw);
        assert_eq!(launch.command, ["openclaw"]);
        let args = vec!["/opt/bin/openclaw".into(), "agent".into(), "--local".into()];
        assert_eq!(
            resolve_launch(AgentKind::Auto, &args).unwrap().command,
            args
        );
        assert_eq!(
            resolve_launch(AgentKind::Auto, &args).unwrap().agent,
            ResolvedAgent::OpenClaw
        );
        assert_eq!(
            resolve_launch(AgentKind::OpenClaw, &["--version".into()])
                .unwrap()
                .command,
            ["openclaw", "--version"]
        );
        assert_eq!(
            resolve_launch(AgentKind::Auto, &["interpreter".into()])
                .unwrap()
                .agent,
            ResolvedAgent::Interpreter
        );
    }

    #[test]
    fn native_config_keeps_route_and_secret_reference_for_each_transport() {
        for api in [
            PiProviderApi::OpenAiResponses,
            PiProviderApi::OpenAiCompletions,
            PiProviderApi::AnthropicMessages,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let provider = PiProviderConfig {
                name: "unused-native-provider-name".into(),
                base_url: "http://127.0.0.1:4321/v1".into(),
                api,
                models: vec!["example/model".into()],
            };
            let path = write_openclaw_config(temp.path(), &provider, "example/model", temp.path())
                .unwrap();
            let config: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            let managed = &config["models"]["providers"][PROVIDER_NAME];
            assert_eq!(managed["api"], api.as_str());
            assert_eq!(managed["models"][0]["id"], "example/model");
            assert_eq!(managed["apiKey"]["id"], API_KEY_ENV_VAR);
            assert_eq!(config["models"]["providers"].as_object().unwrap().len(), 1);
            assert_eq!(config["models"]["mode"], "replace");
            assert!(config["agents"]["entries"]["main"].get("default").is_none());
            assert_eq!(
                config["agents"]["defaults"]["model"]["primary"],
                "ai-fence/example/model"
            );
            assert_eq!(
                config["agents"]["defaults"]["modelPolicy"]["allow"],
                serde_json::json!(["ai-fence/example/model"])
            );
            assert_eq!(config["gateway"]["bind"], "loopback");
            assert_eq!(config["env"]["shellEnv"]["enabled"], false);
        }
    }

    #[test]
    fn gateway_render_is_pure_and_preserves_each_native_protocol() {
        for api in [
            PiProviderApi::OpenAiResponses,
            PiProviderApi::OpenAiCompletions,
            PiProviderApi::AnthropicMessages,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("uncreated-state");
            let provider = PiProviderConfig {
                name: "ignored".into(),
                base_url: "https://fence.example/v1".into(),
                api,
                models: vec!["example/model".into()],
            };
            let config = render_gateway_config(
                &state,
                &provider,
                "example/model",
                &temp.path().join("workspace"),
                19789,
            )
            .unwrap();
            assert!(!state.exists());
            assert_eq!(
                config["models"]["providers"][PROVIDER_NAME]["api"],
                api.as_str()
            );
            assert_eq!(
                config["models"]["providers"][PROVIDER_NAME]["apiKey"]["id"],
                API_KEY_ENV_VAR
            );
            assert_eq!(
                config["agents"]["defaults"]["model"]["primary"],
                "ai-fence/example/model"
            );
            assert_eq!(config["gateway"]["port"], 19789);
            assert_eq!(
                config["gateway"]["auth"]["token"],
                "${OPENCLAW_GATEWAY_TOKEN}"
            );
            assert_eq!(
                config["gateway"]["http"]["endpoints"]["responses"]["files"]["allowUrl"],
                false
            );
            assert_eq!(config["tools"]["deny"], serde_json::json!(["*"]));
            assert_eq!(config["plugins"]["enabled"], false);
            assert_eq!(config["cron"]["enabled"], false);
            assert_eq!(config["agents"]["defaults"]["heartbeat"]["every"], "0m");
        }
    }

    #[test]
    fn gateway_render_refuses_credential_urls_and_unsafe_destinations() {
        let mut provider = PiProviderConfig {
            name: "ignored".into(),
            base_url: "https://fence.example/v1".into(),
            api: PiProviderApi::OpenAiCompletions,
            models: vec!["model".into()],
        };
        for url in [
            "http://fence.example/v1",
            "https://user:secret@fence.example/v1",
            "https://fence.example/v1?token=secret",
            "https://fence.example/v1#secret",
            "https://fence.example/other",
        ] {
            provider.base_url = url.into();
            assert!(render_gateway_config(
                Path::new("/state"),
                &provider,
                "model",
                Path::new("/workspace"),
                19789
            )
            .is_err());
        }
        provider.base_url = "https://fence.example/v1".into();
        assert!(render_gateway_config(
            Path::new("/state"),
            &provider,
            "model",
            Path::new("/workspace"),
            0
        )
        .is_err());
    }

    #[test]
    #[ignore = "requires AI_FENCE_OPENCLAW_TEST_BIN pointing to OpenClaw 2026.9.8"]
    fn native_gateway_accepts_shared_configuration() {
        let binary = std::env::var_os("AI_FENCE_OPENCLAW_TEST_BIN")
            .expect("set AI_FENCE_OPENCLAW_TEST_BIN for this native schema check");
        for api in [
            PiProviderApi::OpenAiResponses,
            PiProviderApi::OpenAiCompletions,
            PiProviderApi::AnthropicMessages,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let state = temp.path().join("state");
            fs::create_dir_all(&state).unwrap();
            let provider = PiProviderConfig {
                name: PROVIDER_NAME.into(),
                base_url: "https://fence.example/v1".into(),
                api,
                models: vec!["fixture-model".into()],
            };
            let config = render_gateway_config(
                &state,
                &provider,
                "fixture-model",
                &temp.path().join("workspace"),
                19789,
            )
            .unwrap();
            fs::write(state.join("openclaw.json"), config.to_string()).unwrap();
            let output = Command::new(&binary)
                .args(["config", "validate", "--json"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", temp.path())
                .env("OPENCLAW_HOME", temp.path())
                .env(STATE_DIR_ENV_VAR, &state)
                .env(CONFIG_PATH_ENV_VAR, state.join("openclaw.json"))
                .env("OPENCLAW_GATEWAY_TOKEN", "synthetic-gateway-token")
                .env(API_KEY_ENV_VAR, "synthetic-fence-key")
                .env("OPENCLAW_NO_AUTO_UPDATE", "1")
                .current_dir(temp.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "native schema rejected {}: {} {}",
                api.as_str(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn environment_drops_ambient_credentials_and_native_overrides() {
        let mut command = Command::new("openclaw");
        command.env("OPENAI_API_KEY", "test-provider-only");
        command.env("AI_FENCE_CODEX_AUTH_JSON", "test-subscription-only");
        command.env(STATE_DIR_ENV_VAR, "/personal/state");
        configure_environment(&mut command, Path::new("/managed/state"), "test-fence-only");
        let env = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(!env.contains_key(std::ffi::OsStr::new("OPENAI_API_KEY")));
        assert!(!env.contains_key(std::ffi::OsStr::new("AI_FENCE_CODEX_AUTH_JSON")));
        assert_eq!(
            env[std::ffi::OsStr::new(API_KEY_ENV_VAR)].unwrap(),
            "test-fence-only"
        );
        assert_eq!(
            env[std::ffi::OsStr::new(STATE_DIR_ENV_VAR)].unwrap(),
            "/managed/state"
        );
    }

    #[test]
    fn profile_sync_keeps_only_native_session_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let profile = resolve_agent_profile(temp.path(), "trial", ResolvedAgent::OpenClaw).unwrap();
        fs::create_dir_all(profile.state_dir.join("sessions")).unwrap();
        fs::write(profile.state_dir.join("sessions/prior.jsonl"), "{}\n").unwrap();
        fs::write(
            profile.state_dir.join("auth-profiles.json"),
            "fixture-personal-auth",
        )
        .unwrap();
        let runtime = temp.path().join("run-state");
        sync_profile_state_to_runtime(&profile, &runtime).unwrap();
        assert!(runtime.join("sessions/prior.jsonl").is_file());
        assert!(!runtime.join("auth-profiles.json").exists());
        fs::write(runtime.join("sessions/new.jsonl"), "{}\n").unwrap();
        fs::write(runtime.join("openclaw.json"), "fixture-runtime-config").unwrap();
        fs::create_dir_all(runtime.join("agents/main/agent")).unwrap();
        fs::write(
            runtime.join("agents/main/agent/auth-profiles.json"),
            "fixture-runtime-auth",
        )
        .unwrap();
        sync_runtime_state_to_profile(&profile, &runtime).unwrap();
        assert!(profile.state_dir.join("sessions/new.jsonl").is_file());
        assert!(!profile.state_dir.join("openclaw.json").exists());
        assert!(!profile.state_dir.join("agents").exists());
    }

    #[test]
    fn refuses_default_personal_state_and_allows_managed_state() {
        let temp = tempfile::tempdir().unwrap();
        if let Some(home) = effective_user_home_dir() {
            assert!(resolve_openclaw_home(temp.path(), Some(&home.join(".openclaw"))).is_err());
            assert!(
                resolve_openclaw_home(temp.path(), Some(&home.join(".openclaw-personal"))).is_err()
            );
        }
        assert_eq!(
            resolve_openclaw_home(temp.path(), None).unwrap(),
            temp.path().canonicalize().unwrap().join(".openclaw")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_symlink_ancestors_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(temp.path().join("missing"), &alias).unwrap();
        assert!(resolve_openclaw_home(temp.path(), Some(&alias.join("state"))).is_err());
    }

    #[test]
    fn native_args_cannot_replace_managed_state_or_auth() {
        for flag in [
            "--profile=personal",
            "--dev",
            "--state-dir=/personal/state",
            "--config=/personal/config.json",
            "--isolated",
            "--auth-env-only",
            "--fallback",
            "--container=untrusted-runtime",
        ] {
            assert!(validate_args(&[flag]).is_err());
        }
        assert!(validate_args(&["agent", "--local", "--agent", "main", "-m", "hello"]).is_ok());
    }
}
