use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use toml::value::Table as TomlTable;

const CODEX_PROJECT_TRUST_FILE: &str = "trusted-projects.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentKind {
    Auto,
    Codex,
    Claude,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAgent {
    Codex,
    Claude,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum CodexAuthSource {
    /// Copy existing ~/.codex/auth.json only when managed auth is missing.
    #[default]
    Auto,
    /// Use auth already present in the managed CODEX_HOME.
    Managed,
    /// Copy ~/.codex/auth.json into the managed CODEX_HOME before launch.
    User,
    /// Do not copy Codex auth state.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexProviderAuth {
    EnvKey,
    EnvBearer { env_key: String },
    OpenAiAuth,
    Command { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub agent: ResolvedAgent,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServer {
    pub name: String,
    pub config: AgentMcpServerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMcpServerConfig {
    StreamableHttp {
        url: String,
        bearer_token_env_var: Option<String>,
        headers: BTreeMap<String, String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexConfigExtras<'a> {
    pub profile: Option<&'a AgentProfile>,
    pub mcp_servers: &'a [AgentMcpServer],
}

impl AgentMcpServer {
    pub fn streamable_http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            config: AgentMcpServerConfig::StreamableHttp {
                url: url.into(),
                bearer_token_env_var: None,
                headers: BTreeMap::new(),
            },
        }
    }

    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            config: AgentMcpServerConfig::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        }
    }

    pub fn with_bearer_token_env_var(mut self, env_var: impl Into<String>) -> Self {
        if let AgentMcpServerConfig::StreamableHttp {
            bearer_token_env_var,
            ..
        } = &mut self.config
        {
            *bearer_token_env_var = Some(env_var.into());
        }
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let AgentMcpServerConfig::StreamableHttp { headers, .. } = &mut self.config {
            headers.insert(name.into(), value.into());
        }
        self
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        if let AgentMcpServerConfig::Stdio { args, .. } = &mut self.config {
            args.push(arg.into());
        }
        self
    }

    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let AgentMcpServerConfig::Stdio { env, .. } = &mut self.config {
            env.insert(name.into(), value.into());
        }
        self
    }

    fn to_codex_toml(&self) -> Result<TomlTable> {
        let mut table = TomlTable::new();
        match &self.config {
            AgentMcpServerConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                headers,
            } => {
                if !headers.is_empty() {
                    anyhow::bail!(
                        "Codex MCP config does not support arbitrary HTTP headers; use bearer_token_env_var or Claude-specific config"
                    );
                }
                table.insert("url".to_string(), toml::Value::String(url.clone()));
                if let Some(env_var) = bearer_token_env_var {
                    table.insert(
                        "bearer_token_env_var".to_string(),
                        toml::Value::String(env_var.clone()),
                    );
                }
            }
            AgentMcpServerConfig::Stdio { command, args, env } => {
                table.insert("command".to_string(), toml::Value::String(command.clone()));
                if !args.is_empty() {
                    table.insert(
                        "args".to_string(),
                        toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
                    );
                }
                if !env.is_empty() {
                    table.insert(
                        "env".to_string(),
                        toml::Value::Table(
                            env.iter()
                                .map(|(key, value)| {
                                    (key.clone(), toml::Value::String(value.clone()))
                                })
                                .collect(),
                        ),
                    );
                }
            }
        }
        Ok(table)
    }

    fn to_claude_json(&self) -> serde_json::Value {
        match &self.config {
            AgentMcpServerConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                headers,
            } => {
                let mut value = serde_json::json!({
                    "type": "http",
                    "url": url,
                });
                let mut rendered_headers = headers.clone();
                if let Some(env_var) = bearer_token_env_var {
                    rendered_headers
                        .entry("Authorization".to_string())
                        .or_insert_with(|| format!("Bearer ${{{env_var}}}"));
                }
                if !rendered_headers.is_empty() {
                    value["headers"] = serde_json::json!(rendered_headers);
                }
                value
            }
            AgentMcpServerConfig::Stdio { command, args, env } => {
                let mut value = serde_json::json!({
                    "type": "stdio",
                    "command": command,
                });
                if !args.is_empty() {
                    value["args"] = serde_json::json!(args);
                }
                if !env.is_empty() {
                    value["env"] = serde_json::json!(env);
                }
                value
            }
        }
    }
}

impl ResolvedAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Generic => "generic command",
        }
    }
}

pub fn resolve_launch(agent: AgentKind, command: &[String]) -> Result<LaunchSpec> {
    let resolved = match agent {
        AgentKind::Auto => detect_agent(command).unwrap_or(ResolvedAgent::Generic),
        AgentKind::Codex => ResolvedAgent::Codex,
        AgentKind::Claude => ResolvedAgent::Claude,
        AgentKind::Generic => ResolvedAgent::Generic,
    };
    let mut command = if command.is_empty() {
        default_command(resolved)?
    } else {
        command.to_vec()
    };
    // If the user passed args without the agent binary (e.g. "exec" instead of "codex exec"),
    // prepend the default binary so the command is always runnable.
    let expected_binary = match resolved {
        ResolvedAgent::Codex => "codex",
        ResolvedAgent::Claude => "claude",
        ResolvedAgent::Generic => "",
    };
    if !expected_binary.is_empty() {
        let first = command.first().and_then(|s| Path::new(s).file_name());
        let first_name = first.and_then(|s| s.to_str());
        if first_name != Some(expected_binary) {
            command.insert(0, expected_binary.to_string());
        }
    }
    Ok(LaunchSpec {
        agent: resolved,
        command,
    })
}

pub fn default_command(agent: ResolvedAgent) -> Result<Vec<String>> {
    match agent {
        ResolvedAgent::Codex => Ok(vec!["codex".to_string()]),
        ResolvedAgent::Claude => Ok(vec!["claude".to_string()]),
        ResolvedAgent::Generic => {
            anyhow::bail!("ai-fence-cli run requires --agent codex/claude or a command after --")
        }
    }
}

pub fn detect_agent(command: &[String]) -> Option<ResolvedAgent> {
    let first = command.first()?;
    let name = Path::new(first).file_name()?.to_str()?;
    match name {
        "codex" => Some(ResolvedAgent::Codex),
        "claude" | "claude-code" => Some(ResolvedAgent::Claude),
        _ => Some(ResolvedAgent::Generic),
    }
}

pub fn default_ai_fence_home() -> Option<PathBuf> {
    std::env::var_os("AI_FENCE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ai-fence")))
}

pub fn resolve_config_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    default_ai_fence_home().context("HOME is not set and AI_FENCE_HOME was not provided")
}

pub fn resolve_template_dir(config_dir: &Path, explicit_fence_dir: Option<&Path>) -> PathBuf {
    explicit_fence_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.to_path_buf())
}

pub fn create_run_dir(config_dir: &Path) -> Result<PathBuf> {
    let run_dir = config_dir
        .join("runs")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    Ok(run_dir)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub name: String,
    pub agent: ResolvedAgent,
    pub root_dir: PathBuf,
    pub state_dir: PathBuf,
    pub lock_path: PathBuf,
    pub metadata_path: PathBuf,
}

#[derive(Debug)]
pub struct AgentProfileLock {
    lock_path: PathBuf,
}

impl Drop for AgentProfileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[derive(Debug, Serialize)]
pub struct AgentProfileMetadata<'a> {
    pub profile: &'a str,
    pub agent: &'a str,
    pub auth_lane: &'a str,
}

pub fn sanitize_profile_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("agent profile name must not be empty");
    }
    if trimmed == "." || trimmed == ".." || trimmed.starts_with('.') {
        anyhow::bail!("agent profile name must not be hidden or a traversal segment");
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!(
            "agent profile name may only contain ASCII letters, numbers, '.', '-', and '_'"
        );
    }
    Ok(trimmed.to_string())
}

pub fn resolve_agent_profile(
    config_dir: &Path,
    profile_name: &str,
    agent: ResolvedAgent,
) -> Result<AgentProfile> {
    let agent_dir = match agent {
        ResolvedAgent::Codex => "codex",
        ResolvedAgent::Claude => "claude",
        ResolvedAgent::Generic => {
            anyhow::bail!("managed agent profiles are only supported for Codex and Claude Code")
        }
    };
    let name = sanitize_profile_name(profile_name)?;
    let root_dir = config_dir.join("profiles").join(&name).join(agent_dir);
    Ok(AgentProfile {
        name,
        agent,
        state_dir: root_dir.join("state"),
        lock_path: root_dir.join(".sync.lock"),
        metadata_path: root_dir.join("metadata.json"),
        root_dir,
    })
}

pub fn acquire_agent_profile_lock(profile: &AgentProfile) -> Result<AgentProfileLock> {
    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let mut lock = match create_agent_profile_lock_file(profile) {
        Ok(lock) => lock,
        Err(err)
            if err.kind() == ErrorKind::AlreadyExists
                && stale_agent_profile_lock(&profile.lock_path)? =>
        {
            fs::remove_file(&profile.lock_path).with_context(|| {
                format!("failed to remove stale {}", profile.lock_path.display())
            })?;
            create_agent_profile_lock_file(profile).with_context(|| {
                format!(
                    "agent profile '{}' for {} is already in use or locked at {}",
                    profile.name,
                    profile.agent.as_str(),
                    profile.lock_path.display()
                )
            })?
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "agent profile '{}' for {} is already in use or locked at {}",
                    profile.name,
                    profile.agent.as_str(),
                    profile.lock_path.display()
                )
            });
        }
    };
    writeln!(lock, "pid={}", std::process::id())
        .with_context(|| format!("failed to write {}", profile.lock_path.display()))?;
    Ok(AgentProfileLock {
        lock_path: profile.lock_path.clone(),
    })
}

fn create_agent_profile_lock_file(profile: &AgentProfile) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&profile.lock_path)
}

fn stale_agent_profile_lock(lock_path: &Path) -> Result<bool> {
    let data = fs::read_to_string(lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
    let Some(pid) = data
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.trim().parse::<u32>().ok())
    else {
        return Ok(false);
    };
    Ok(!process_exists(pid))
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(_pid: u32) -> bool {
    true
}

pub fn write_agent_profile_metadata(
    profile: &AgentProfile,
    metadata: &AgentProfileMetadata<'_>,
) -> Result<()> {
    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let data = serde_json::to_vec_pretty(metadata).context("failed to encode profile metadata")?;
    fs::write(&profile.metadata_path, data)
        .with_context(|| format!("failed to write {}", profile.metadata_path.display()))
}

pub fn sync_profile_state_to_runtime(
    profile: &AgentProfile,
    runtime_agent_dir: &Path,
) -> Result<()> {
    let _lock = acquire_agent_profile_lock(profile)?;
    if profile.agent == ResolvedAgent::Codex {
        remove_codex_sqlite_profile_state(&profile.state_dir)?;
    }
    sync_agent_state(profile.agent, &profile.state_dir, runtime_agent_dir)
}

pub fn sync_runtime_state_to_profile(
    profile: &AgentProfile,
    runtime_agent_dir: &Path,
) -> Result<()> {
    let _lock = acquire_agent_profile_lock(profile)?;
    if profile.agent == ResolvedAgent::Codex {
        remove_codex_sqlite_profile_state(&profile.state_dir)?;
    }
    sync_agent_state(profile.agent, runtime_agent_dir, &profile.state_dir)?;
    if profile.agent == ResolvedAgent::Codex {
        persist_codex_project_trust(runtime_agent_dir, &profile.state_dir)?;
    }
    Ok(())
}

fn sync_agent_state(agent: ResolvedAgent, source_dir: &Path, target_dir: &Path) -> Result<()> {
    if !source_dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("failed to read {}", source_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", source_dir.display()))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if should_sync_agent_state_entry(agent, &file_name) {
            copy_profile_path(&entry.path(), &target_dir.join(file_name.as_ref()))?;
        }
    }
    Ok(())
}

fn should_sync_agent_state_entry(agent: ResolvedAgent, name: &str) -> bool {
    match agent {
        ResolvedAgent::Codex => {
            matches!(
                name,
                "history.jsonl"
                    | "installation_id"
                    | "models_cache.json"
                    | "sessions"
                    | "shell_snapshots"
                    | CODEX_PROJECT_TRUST_FILE
            )
        }
        ResolvedAgent::Claude => matches!(name, "history.jsonl" | "projects" | "todos"),
        ResolvedAgent::Generic => false,
    }
}

fn persist_codex_project_trust(runtime_codex_home: &Path, profile_state_dir: &Path) -> Result<()> {
    let config_path = runtime_codex_home.join("config.toml");
    if !config_path.is_file() {
        return Ok(());
    }
    let data = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let value = data
        .parse::<toml::Value>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let Some(projects) = value.get("projects").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    let mut trusted_projects = TomlTable::new();
    for (path, project) in projects {
        let Some(project) = project.as_table() else {
            continue;
        };
        let Some(trust_level) = project.get("trust_level").and_then(toml::Value::as_str) else {
            continue;
        };
        if trust_level != "trusted" && trust_level != "untrusted" {
            continue;
        }
        let mut persisted_project = TomlTable::new();
        persisted_project.insert(
            "trust_level".to_string(),
            toml::Value::String(trust_level.to_string()),
        );
        trusted_projects.insert(path.clone(), toml::Value::Table(persisted_project));
    }

    if trusted_projects.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(profile_state_dir)
        .with_context(|| format!("failed to create {}", profile_state_dir.display()))?;
    let mut root = TomlTable::new();
    root.insert("projects".to_string(), toml::Value::Table(trusted_projects));
    let target_path = profile_state_dir.join(CODEX_PROJECT_TRUST_FILE);
    fs::write(
        &target_path,
        toml::to_string_pretty(&toml::Value::Table(root)).context("failed to render TOML")?,
    )
    .with_context(|| format!("failed to write {}", target_path.display()))
}

fn codex_sqlite_state_file(name: &str) -> bool {
    ["state_", "goals_", "memories_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && (name.contains(".sqlite") || name.ends_with(".db"))
}

fn remove_codex_sqlite_profile_state(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(state_dir)
        .with_context(|| format!("failed to read {}", state_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", state_dir.display()))?;
        let name = entry.file_name();
        if codex_sqlite_state_file(&name.to_string_lossy()) && entry.path().is_file() {
            let path = entry.path();
            fs::remove_file(&path).with_context(|| {
                format!("failed to remove transient Codex state {}", path.display())
            })?;
            tracing::info!(
                path = %path.display(),
                "removed transient Codex SQLite profile state"
            );
        }
    }
    Ok(())
}

fn copy_profile_path(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        for entry in
            fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
        {
            let entry = entry.with_context(|| format!("failed to read {}", source.display()))?;
            copy_profile_path(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(source, target).with_context(|| {
            format!(
                "failed to copy profile state {} to {}",
                source.display(),
                target.display()
            )
        })?;
        preserve_file_timestamps(target, &metadata)?;
    }
    Ok(())
}

fn preserve_file_timestamps(target: &Path, source_metadata: &fs::Metadata) -> Result<()> {
    let mut times = FileTimes::new();
    let mut has_time = false;
    if let Ok(modified) = source_metadata.modified() {
        times = times.set_modified(modified);
        has_time = true;
    }
    if let Ok(accessed) = source_metadata.accessed() {
        times = times.set_accessed(accessed);
        has_time = true;
    }
    if !has_time {
        return Ok(());
    }

    OpenOptions::new()
        .write(true)
        .open(target)
        .with_context(|| format!("failed to open {} to restore timestamps", target.display()))?
        .set_times(times)
        .with_context(|| format!("failed to restore timestamps on {}", target.display()))
}

pub fn default_user_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

pub fn normalize_path_lexical(path: &Path) -> Result<PathBuf> {
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

pub fn resolve_codex_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let codex_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".codex"));
    if let Some(default_home) = default_user_codex_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&codex_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default CODEX_HOME {}; use the managed {managed_dir_label} directory or copy auth with --codex-auth-source user",
                default_home.display()
            );
        }
    }
    Ok(codex_home)
}

pub fn hydrate_codex_auth(codex_home: &Path, source: CodexAuthSource) -> Result<()> {
    if matches!(source, CodexAuthSource::None | CodexAuthSource::Managed) {
        return Ok(());
    }
    let Some(user_codex_home) = default_user_codex_home() else {
        return Ok(());
    };
    let source_auth = user_codex_home.join("auth.json");
    if !source_auth.is_file() {
        return Ok(());
    }
    let target_auth = codex_home.join("auth.json");
    if source == CodexAuthSource::Auto && target_auth.exists() {
        return Ok(());
    }
    fs::create_dir_all(codex_home)
        .with_context(|| format!("failed to create CODEX_HOME {}", codex_home.display()))?;
    fs::copy(&source_auth, &target_auth).with_context(|| {
        format!(
            "failed to copy Codex auth from {} to {}",
            source_auth.display(),
            target_auth.display()
        )
    })?;
    Ok(())
}

pub fn write_codex_config(
    codex_home: &Path,
    proxy_base: &str,
    model: Option<&str>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
) -> Result<()> {
    write_codex_config_with_mcp(
        codex_home,
        proxy_base,
        model,
        template_dir,
        yolo,
        provider_auth,
        &[],
    )
}

pub fn write_codex_config_with_mcp(
    codex_home: &Path,
    proxy_base: &str,
    model: Option<&str>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
    mcp_servers: &[AgentMcpServer],
) -> Result<()> {
    write_codex_config_with_profile_and_mcp(
        codex_home,
        proxy_base,
        model,
        template_dir,
        yolo,
        provider_auth,
        CodexConfigExtras {
            profile: None,
            mcp_servers,
        },
    )
}

pub fn write_codex_config_with_profile(
    codex_home: &Path,
    proxy_base: &str,
    model: Option<&str>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
    profile: Option<&AgentProfile>,
) -> Result<()> {
    write_codex_config_with_profile_and_mcp(
        codex_home,
        proxy_base,
        model,
        template_dir,
        yolo,
        provider_auth,
        CodexConfigExtras {
            profile,
            mcp_servers: &[],
        },
    )
}

pub fn write_codex_config_with_profile_and_mcp(
    codex_home: &Path,
    proxy_base: &str,
    model: Option<&str>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
    extras: CodexConfigExtras<'_>,
) -> Result<()> {
    fs::create_dir_all(codex_home)
        .with_context(|| format!("failed to create CODEX_HOME {}", codex_home.display()))?;

    let mut config = read_codex_config_template(template_dir)?.unwrap_or_default();
    if let Some(profile_config) = read_codex_profile_config_template(extras.profile)? {
        merge_codex_config_overlay(&mut config, profile_config);
    }

    insert_string_if_missing(&mut config, "cli_auth_credentials_store", "file");
    let features = ensure_table(&mut config, "features");
    features
        .entry("responses_websockets".to_string())
        .or_insert(toml::Value::Boolean(false));

    if let Some(m) = model {
        insert_string(&mut config, "model", m);
        let effective_model = config
            .get("model")
            .and_then(toml::Value::as_str)
            .unwrap_or(m);
        if should_write_codex_model_catalog(effective_model)
            && !config.contains_key("model_catalog_json")
        {
            let catalog_path = codex_home.join("model-catalog.json");
            write_codex_model_catalog(&catalog_path, effective_model)?;
            config.insert(
                "model_catalog_json".to_string(),
                toml::Value::String(catalog_path.to_string_lossy().to_string()),
            );
        }
    }

    match provider_auth {
        CodexProviderAuth::EnvKey => {
            configure_codex_proxy_provider(&mut config, proxy_base);
            let provider = ensure_model_provider(&mut config);
            provider.insert(
                "env_key".to_string(),
                toml::Value::String("OPENAI_API_KEY".to_string()),
            );
            provider.remove("requires_openai_auth");
            provider.remove("auth");
            config.insert(
                "preferred_auth_method".to_string(),
                toml::Value::String("apikey".to_string()),
            );
        }
        CodexProviderAuth::EnvBearer { env_key } => {
            configure_codex_proxy_provider(&mut config, proxy_base);
            let provider = ensure_model_provider(&mut config);
            provider.insert("env_key".to_string(), toml::Value::String(env_key));
            provider.remove("requires_openai_auth");
            provider.remove("auth");
        }
        CodexProviderAuth::OpenAiAuth => {
            configure_codex_proxy_provider(&mut config, proxy_base);
            config.insert(
                "preferred_auth_method".to_string(),
                toml::Value::String("chatgpt".to_string()),
            );
            let provider = ensure_model_provider(&mut config);
            provider.insert(
                "requires_openai_auth".to_string(),
                toml::Value::Boolean(true),
            );
            provider.remove("env_key");
            provider.remove("auth");
        }
        CodexProviderAuth::Command { command, args } => {
            configure_codex_proxy_provider(&mut config, proxy_base);
            let provider = ensure_model_provider(&mut config);
            provider.remove("env_key");
            provider.remove("requires_openai_auth");
            let mut auth = TomlTable::new();
            auth.insert("command".to_string(), toml::Value::String(command));
            if !args.is_empty() {
                auth.insert(
                    "args".to_string(),
                    toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
                );
            }
            provider.insert("auth".to_string(), toml::Value::Table(auth));
        }
    }

    merge_codex_project_trust(&mut config, &codex_home.join(CODEX_PROJECT_TRUST_FILE))?;

    if yolo {
        if let Ok(cwd) = std::env::current_dir() {
            let project_path = cwd.to_string_lossy().replace('\\', "/");
            let projects = ensure_table(&mut config, "projects");
            let project = projects
                .entry(project_path)
                .or_insert_with(|| toml::Value::Table(TomlTable::new()));
            if !project.is_table() {
                *project = toml::Value::Table(TomlTable::new());
            }
            project.as_table_mut().unwrap().insert(
                "trust_level".to_string(),
                toml::Value::String("trusted".to_string()),
            );
        }
    }
    merge_codex_mcp_servers(&mut config, extras.mcp_servers)?;

    fs::write(
        codex_home.join("config.toml"),
        toml::to_string_pretty(&toml::Value::Table(config)).context("failed to render TOML")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            codex_home.join("config.toml").display()
        )
    })
}

pub fn codex_profile_config_path(profile: &AgentProfile) -> PathBuf {
    profile.root_dir.join("config.toml")
}

pub fn upsert_codex_profile_mcp_server(
    profile: &AgentProfile,
    server: &AgentMcpServer,
) -> Result<()> {
    if profile.agent != ResolvedAgent::Codex {
        anyhow::bail!("Codex profile MCP config requires a Codex agent profile");
    }
    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let mut config = read_codex_profile_config_template(Some(profile))?.unwrap_or_default();
    merge_codex_mcp_servers(&mut config, std::slice::from_ref(server))?;
    let target = codex_profile_config_path(profile);
    fs::write(
        &target,
        toml::to_string_pretty(&toml::Value::Table(config)).context("failed to render TOML")?,
    )
    .with_context(|| format!("failed to write {}", target.display()))
}

fn merge_codex_project_trust(config: &mut TomlTable, trust_path: &Path) -> Result<()> {
    if !trust_path.is_file() {
        return Ok(());
    }
    let data = fs::read_to_string(trust_path)
        .with_context(|| format!("failed to read {}", trust_path.display()))?;
    let value = data
        .parse::<toml::Value>()
        .with_context(|| format!("failed to parse {}", trust_path.display()))?;
    let Some(projects) = value.get("projects").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    let config_projects = ensure_table(config, "projects");
    for (path, project) in projects {
        let Some(project) = project.as_table() else {
            continue;
        };
        let Some(trust_level) = project.get("trust_level").and_then(toml::Value::as_str) else {
            continue;
        };
        if trust_level != "trusted" && trust_level != "untrusted" {
            continue;
        }
        let entry = config_projects
            .entry(path.clone())
            .or_insert_with(|| toml::Value::Table(TomlTable::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(TomlTable::new());
        }
        entry.as_table_mut().unwrap().insert(
            "trust_level".to_string(),
            toml::Value::String(trust_level.to_string()),
        );
    }
    Ok(())
}

fn read_codex_config_template(template_dir: Option<&Path>) -> Result<Option<TomlTable>> {
    let Some(dir) = template_dir else {
        return Ok(None);
    };
    let Some(template) = read_first_existing_template(&[
        dir.join(".codex").join("config.toml"),
        dir.join("codex-config.toml"),
    ])?
    else {
        return Ok(None);
    };
    let value = template.parse::<toml::Value>().with_context(|| {
        format!(
            "failed to parse Codex config template under {}",
            dir.display()
        )
    })?;
    Ok(match value {
        toml::Value::Table(table) => Some(table),
        _ => None,
    })
}

fn read_codex_profile_config_template(profile: Option<&AgentProfile>) -> Result<Option<TomlTable>> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let Some(template) = read_first_existing_template(&[
        codex_profile_config_path(profile),
        profile.root_dir.join(".codex").join("config.toml"),
        profile.root_dir.join("codex-config.toml"),
    ])?
    else {
        return Ok(None);
    };
    let value = template.parse::<toml::Value>().with_context(|| {
        format!(
            "failed to parse Codex profile config template under {}",
            profile.root_dir.display()
        )
    })?;
    Ok(match value {
        toml::Value::Table(table) => Some(table),
        _ => None,
    })
}

fn merge_codex_config_overlay(config: &mut TomlTable, overlay: TomlTable) {
    for (key, value) in overlay {
        if key == "mcp_servers" {
            merge_toml_named_tables_replace_entries(config, key, value);
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

fn merge_toml_named_tables_replace_entries(
    parent: &mut TomlTable,
    key: impl Into<String>,
    overlay: toml::Value,
) {
    let key = key.into();
    let toml::Value::Table(overlay_table) = overlay else {
        parent.insert(key, overlay);
        return;
    };
    let base = ensure_table(parent, &key);
    for (name, value) in overlay_table {
        base.insert(name, value);
    }
}

fn merge_codex_mcp_servers(config: &mut TomlTable, servers: &[AgentMcpServer]) -> Result<()> {
    if servers.is_empty() {
        return Ok(());
    }
    let mcp_servers = ensure_table(config, "mcp_servers");
    for server in servers {
        if server.name.trim().is_empty() {
            anyhow::bail!("MCP server name must not be empty");
        }
        mcp_servers.insert(
            server.name.clone(),
            toml::Value::Table(server.to_codex_toml()?),
        );
    }
    Ok(())
}

fn read_claude_settings_template(template_dir: Option<&Path>) -> Result<Option<serde_json::Value>> {
    let Some(dir) = template_dir else {
        return Ok(None);
    };
    let Some(template) = read_first_existing_template(&[
        dir.join(".claude").join("settings.json"),
        dir.join("claude-settings.json"),
    ])?
    else {
        return Ok(None);
    };
    serde_json::from_str(&template)
        .with_context(|| format!("invalid Claude settings JSON under {}", dir.display()))
        .map(Some)
}

fn read_claude_user_config_template(
    template_dir: Option<&Path>,
) -> Result<Option<serde_json::Value>> {
    let Some(dir) = template_dir else {
        return Ok(None);
    };
    let Some(template) = read_first_existing_template(&[
        dir.join(".claude").join(".claude.json"),
        dir.join("claude-user-config.json"),
    ])?
    else {
        return Ok(None);
    };
    serde_json::from_str(&template)
        .with_context(|| format!("invalid Claude user config JSON under {}", dir.display()))
        .map(Some)
}

fn read_first_existing_template(paths: &[PathBuf]) -> Result<Option<String>> {
    for path in paths {
        if path.is_file() {
            return fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))
                .map(Some);
        }
    }
    Ok(None)
}

fn insert_string_if_missing(config: &mut TomlTable, key: &str, value: &str) {
    config
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::String(value.to_string()));
}

fn insert_string(config: &mut TomlTable, key: &str, value: &str) {
    config.insert(key.to_string(), toml::Value::String(value.to_string()));
}

fn ensure_table<'a>(parent: &'a mut TomlTable, key: &str) -> &'a mut TomlTable {
    let value = parent
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Table(TomlTable::new()));
    if !value.is_table() {
        *value = toml::Value::Table(TomlTable::new());
    }
    value.as_table_mut().unwrap()
}

fn ensure_model_provider(config: &mut TomlTable) -> &mut TomlTable {
    let providers = ensure_table(config, "model_providers");
    let provider = providers
        .entry("ai_fence".to_string())
        .or_insert_with(|| toml::Value::Table(TomlTable::new()));
    if !provider.is_table() {
        *provider = toml::Value::Table(TomlTable::new());
    }
    provider.as_table_mut().unwrap()
}

fn configure_codex_proxy_provider(config: &mut TomlTable, proxy_base: &str) {
    config.remove("openai_base_url");
    config.insert(
        "model_provider".to_string(),
        toml::Value::String("ai_fence".to_string()),
    );
    let provider = ensure_model_provider(config);
    insert_string_if_missing(provider, "name", "AI Fence");
    provider.insert(
        "base_url".to_string(),
        toml::Value::String(format!("{proxy_base}/v1")),
    );
    provider.insert(
        "wire_api".to_string(),
        toml::Value::String("responses".to_string()),
    );
    provider.insert(
        "supports_websockets".to_string(),
        toml::Value::Boolean(true),
    );
}

fn should_write_codex_model_catalog(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && (model.contains('/') || model.contains(':'))
}

fn write_codex_model_catalog(path: &Path, model: &str) -> Result<()> {
    let catalog = serde_json::json!({
        "models": [codex_model_catalog_entry(model)]
    });
    fs::write(
        path,
        serde_json::to_string_pretty(&catalog).context("failed to render Codex model catalog")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

fn codex_model_catalog_entry(model: &str) -> serde_json::Value {
    let resolved = ai_fence_model_metadata::resolve_builtin_metadata(model);
    let metadata = resolved.as_ref().map(|value| &value.metadata);
    let display_name = metadata
        .and_then(|metadata| metadata.display_name.as_deref())
        .unwrap_or(model);
    let context_window = metadata
        .and_then(|metadata| metadata.max_input_tokens)
        .unwrap_or(128_000);
    let supports_reasoning = metadata
        .and_then(|metadata| metadata.supports_reasoning)
        .unwrap_or(true);
    let supports_parallel_tools = metadata
        .and_then(|metadata| metadata.supports_parallel_tools)
        .unwrap_or(true);
    let supports_vision = metadata
        .and_then(|metadata| metadata.supports_vision)
        .unwrap_or(false);
    serde_json::json!({
        "slug": model,
        "display_name": display_name,
        "description": "Model routed through AI Fence.",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {
                "effort": "low",
                "description": "Fast responses with lighter reasoning"
            },
            {
                "effort": "medium",
                "description": "Balances speed and reasoning depth for everyday tasks"
            },
            {
                "effort": "high",
                "description": "Greater reasoning depth for complex problems"
            }
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 50,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": "You are Codex, a coding agent. You and the user share one workspace, and your job is to collaborate with them until their goal is genuinely handled.",
        "model_messages": null,
        "supports_reasoning_summaries": supports_reasoning,
        "default_reasoning_summary": "none",
        "support_verbosity": true,
        "default_verbosity": "medium",
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text",
        "truncation_policy": {
            "mode": "tokens",
            "limit": 10000
        },
        "supports_parallel_tool_calls": supports_parallel_tools,
        "supports_image_detail_original": supports_vision,
        "context_window": context_window,
        "max_context_window": context_window,
        "auto_compact_token_limit": null,
        "effective_context_window_percent": 90,
        "experimental_supported_tools": [],
        "input_modalities": ["text"]
    })
}

pub fn write_agent_env_file(
    agent_dir: &Path,
    proxy_base: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let mut env = format!(
        "OPENAI_BASE_URL={proxy_base}/v1\nOPENAI_API_BASE={proxy_base}/v1\nANTHROPIC_BASE_URL={proxy_base}\n"
    );
    if let Some(api_key) = api_key {
        env.push_str(&format!(
            "OPENAI_API_KEY={api_key}\nANTHROPIC_API_KEY={api_key}\nANTHROPIC_AUTH_TOKEN={api_key}\n"
        ));
    }
    fs::write(agent_dir.join("env"), env)
        .with_context(|| format!("failed to write {}", agent_dir.join("env").display()))
}

pub fn write_claude_settings(
    claude_dir: &Path,
    proxy_base: &str,
    model: Option<&str>,
    template_dir: Option<&Path>,
    yolo: bool,
) -> Result<()> {
    let mut settings: serde_json::Value =
        read_claude_settings_template(template_dir)?.unwrap_or_else(|| serde_json::json!({}));

    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let obj = settings.as_object_mut().unwrap();

    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
    if let Some(env_obj) = env.as_object_mut() {
        env_obj.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            serde_json::json!(proxy_base),
        );
        if let Some(m) = model {
            env_obj.insert(
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                serde_json::json!(m),
            );
            env_obj.insert(
                "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                serde_json::json!(m),
            );
            env_obj.insert(
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                serde_json::json!(m),
            );
        }
    }

    if yolo {
        obj.insert(
            "skipDangerousModePermissionPrompt".to_string(),
            serde_json::json!(true),
        );
        let perms = obj
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(perms_obj) = perms.as_object_mut() {
            perms_obj.insert(
                "allow".to_string(),
                serde_json::json!(["Bash", "Edit", "Write", "Read"]),
            );
        }
    }

    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            claude_dir.join("settings.json").display()
        )
    })
}

pub fn write_claude_user_config(claude_dir: &Path, template_dir: Option<&Path>) -> Result<()> {
    write_claude_user_config_with_mcp(claude_dir, template_dir, &[])
}

pub fn write_claude_user_config_with_mcp(
    claude_dir: &Path,
    template_dir: Option<&Path>,
    mcp_servers: &[AgentMcpServer],
) -> Result<()> {
    let mut user_config =
        read_claude_user_config_template(template_dir)?.unwrap_or_else(|| serde_json::json!({}));
    if !user_config.is_object() {
        anyhow::bail!("Claude user config template must be a JSON object");
    }
    merge_claude_mcp_servers(&mut user_config, mcp_servers)?;
    if user_config.as_object().is_some_and(|obj| obj.is_empty()) {
        return Ok(());
    }
    fs::create_dir_all(claude_dir)
        .with_context(|| format!("failed to create {}", claude_dir.display()))?;
    fs::write(
        claude_dir.join(".claude.json"),
        serde_json::to_string_pretty(&user_config)
            .context("failed to render Claude user config")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            claude_dir.join(".claude.json").display()
        )
    })
}

fn merge_claude_mcp_servers(
    user_config: &mut serde_json::Value,
    servers: &[AgentMcpServer],
) -> Result<()> {
    if servers.is_empty() {
        return Ok(());
    }
    let obj = user_config
        .as_object_mut()
        .context("Claude user config template must be a JSON object")?;
    let mcp_servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !mcp_servers.is_object() {
        anyhow::bail!("Claude user config mcpServers must be a JSON object");
    }
    let mcp_obj = mcp_servers.as_object_mut().unwrap();
    for server in servers {
        if server.name.trim().is_empty() {
            anyhow::bail!("MCP server name must not be empty");
        }
        mcp_obj.insert(server.name.clone(), server.to_claude_json());
    }
    Ok(())
}

#[derive(Serialize)]
pub struct RuntimeMetadata<'a> {
    pub agent: &'a str,
    pub proxy_base_url: &'a str,
    pub openai_base_url: String,
    pub anthropic_base_url: &'a str,
    pub codex_home: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_launch_prepends_binary_when_args_omit_it() {
        let launch = resolve_launch(AgentKind::Codex, &["exec".to_string()]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Codex);
        assert_eq!(
            launch.command,
            vec!["codex".to_string(), "exec".to_string()]
        );
    }

    #[test]
    fn codex_launch_defaults_to_binary_only() {
        let launch = resolve_launch(AgentKind::Codex, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Codex);
        assert_eq!(launch.command, vec!["codex".to_string()]);
    }

    #[test]
    fn codex_launch_keeps_binary_only_when_only_binary_is_given() {
        let launch = resolve_launch(AgentKind::Codex, &["codex".to_string()]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Codex);
        assert_eq!(launch.command, vec!["codex".to_string()]);
    }

    #[test]
    fn auto_agent_detects_wrapped_codex_command() {
        let command = vec!["/usr/local/bin/codex".to_string(), "exec".to_string()];
        let launch = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Codex);
        assert_eq!(launch.command, command);
    }

    #[test]
    fn generic_agent_requires_command() {
        let error = resolve_launch(AgentKind::Generic, &[]).expect_err("error");
        assert!(error.to_string().contains("requires --agent codex/claude"));
    }

    #[test]
    fn write_codex_config_uses_env_key_provider_for_api_key_proxy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            Some("glm-5.1"),
            None,
            true,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(config.contains("model = \"glm-5.1\""));
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("env_key = \"OPENAI_API_KEY\""));
        assert!(config.contains("supports_websockets = true"));
        assert!(config.contains("preferred_auth_method = \"apikey\""));
        assert!(!config.contains("openai_base_url"));
        assert!(!config.contains("requires_openai_auth"));
        assert!(config.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn resolve_codex_home_defaults_to_run_scoped_dot_codex() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let codex_home = resolve_codex_home(&run_dir, None, ".ai-fence/runs").expect("codex home");

        assert_eq!(codex_home, run_dir.join(".codex"));
    }

    #[test]
    fn create_run_dir_uses_runs_subdirectory() {
        let temp = tempfile::tempdir().expect("tempdir");

        let run_dir = create_run_dir(temp.path()).expect("run dir");

        assert!(run_dir.starts_with(temp.path().join("runs")));
        assert!(run_dir.is_dir());
    }

    #[test]
    fn resolve_config_dir_prefers_explicit_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let explicit = temp.path().join("custom-ai-fence");

        let config_dir = resolve_config_dir(Some(&explicit)).expect("config dir");

        assert_eq!(config_dir, explicit);
    }

    #[test]
    fn resolve_template_dir_defaults_to_config_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let explicit = temp.path().join("templates");

        assert_eq!(resolve_template_dir(&config_dir, None), config_dir);
        assert_eq!(resolve_template_dir(&config_dir, Some(&explicit)), explicit);
    }

    #[test]
    fn agent_profile_names_are_sanitized() {
        assert_eq!(
            sanitize_profile_name("work_1.prod").expect("profile"),
            "work_1.prod"
        );
        assert!(sanitize_profile_name("").is_err());
        assert!(sanitize_profile_name("../prod").is_err());
        assert!(sanitize_profile_name(".hidden").is_err());
        assert!(sanitize_profile_name("work/prod").is_err());
    }

    #[test]
    fn resolve_agent_profile_uses_managed_layout() {
        let temp = tempfile::tempdir().expect("tempdir");

        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");

        assert_eq!(profile.name, "default");
        assert_eq!(profile.agent, ResolvedAgent::Codex);
        assert_eq!(
            profile.root_dir,
            temp.path().join("profiles").join("default").join("codex")
        );
        assert_eq!(profile.state_dir, profile.root_dir.join("state"));
        assert_eq!(profile.lock_path, profile.root_dir.join(".sync.lock"));
        assert_eq!(
            profile.metadata_path,
            profile.root_dir.join("metadata.json")
        );
    }

    #[test]
    fn agent_profile_lock_blocks_parallel_sync() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Claude).expect("profile");

        let lock = acquire_agent_profile_lock(&profile).expect("first lock");

        assert!(acquire_agent_profile_lock(&profile).is_err());
        drop(lock);
        assert!(acquire_agent_profile_lock(&profile).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_profile_lock_reclaims_stale_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Claude).expect("profile");
        fs::create_dir_all(&profile.root_dir).expect("profile dir");
        fs::write(&profile.lock_path, "pid=4294967295\n").expect("stale lock");

        let _lock = acquire_agent_profile_lock(&profile).expect("reclaimed lock");

        let lock_data = fs::read_to_string(&profile.lock_path).expect("lock");
        assert!(lock_data.contains(&format!("pid={}", std::process::id())));
    }

    #[test]
    fn codex_profile_sync_preserves_durable_state_and_excludes_sqlite_and_secrets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");
        let runtime = temp.path().join("run").join(".codex");
        fs::create_dir_all(runtime.join("shell_snapshots")).expect("runtime dirs");
        fs::write(runtime.join("history.jsonl"), "{}\n").expect("history");
        fs::write(runtime.join("state_5.sqlite"), "state").expect("state db");
        fs::write(runtime.join("goals_1.sqlite-wal"), "wal").expect("wal");
        fs::write(runtime.join("logs_2.sqlite"), "local logs").expect("logs db");
        fs::write(runtime.join("logs_2.sqlite-wal"), "local logs wal").expect("logs wal");
        fs::write(runtime.join("models_cache.json"), "{}").expect("models");
        fs::create_dir_all(runtime.join("sessions").join("2026").join("06").join("08"))
            .expect("session dirs");
        let session_file = runtime
            .join("sessions")
            .join("2026")
            .join("06")
            .join("08")
            .join("rollout-test.jsonl");
        fs::write(&session_file, "{}\n").expect("session");
        let session_modified_at =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_123);
        OpenOptions::new()
            .write(true)
            .open(&session_file)
            .expect("open session")
            .set_times(FileTimes::new().set_modified(session_modified_at))
            .expect("set session timestamp");
        fs::write(runtime.join("shell_snapshots").join("snap"), "snapshot").expect("snapshot");
        fs::write(runtime.join("auth.json"), "secret").expect("auth");
        fs::write(runtime.join("config.toml"), "secret_config = true").expect("config");

        fs::create_dir_all(&profile.state_dir).expect("profile state dir");
        fs::write(profile.state_dir.join("state_4.sqlite"), "stale state")
            .expect("stale profile state db");
        fs::write(profile.state_dir.join("state_4.sqlite-wal"), "stale wal")
            .expect("stale profile state wal");

        sync_runtime_state_to_profile(&profile, &runtime).expect("sync out");
        let restored = temp.path().join("restored").join(".codex");
        sync_profile_state_to_runtime(&profile, &restored).expect("sync in");

        assert_eq!(
            fs::read_to_string(restored.join("history.jsonl")).expect("history"),
            "{}\n"
        );
        assert_eq!(
            fs::read_to_string(restored.join("shell_snapshots").join("snap")).expect("snapshot"),
            "snapshot"
        );
        assert_eq!(
            fs::read_to_string(
                restored
                    .join("sessions")
                    .join("2026")
                    .join("06")
                    .join("08")
                    .join("rollout-test.jsonl")
            )
            .expect("session"),
            "{}\n"
        );
        assert_eq!(
            fs::metadata(
                profile
                    .state_dir
                    .join("sessions")
                    .join("2026")
                    .join("06")
                    .join("08")
                    .join("rollout-test.jsonl")
            )
            .expect("profile session metadata")
            .modified()
            .expect("profile session modified"),
            session_modified_at
        );
        assert_eq!(
            fs::metadata(
                restored
                    .join("sessions")
                    .join("2026")
                    .join("06")
                    .join("08")
                    .join("rollout-test.jsonl")
            )
            .expect("restored session metadata")
            .modified()
            .expect("restored session modified"),
            session_modified_at
        );
        assert!(!profile.state_dir.join("auth.json").exists());
        assert!(!profile.state_dir.join("config.toml").exists());
        assert!(!profile.state_dir.join("logs_2.sqlite").exists());
        assert!(!profile.state_dir.join("logs_2.sqlite-wal").exists());
        assert!(!profile.state_dir.join("state_4.sqlite").exists());
        assert!(!profile.state_dir.join("state_4.sqlite-wal").exists());
        assert!(!profile.state_dir.join("state_5.sqlite").exists());
        assert!(!profile.state_dir.join("goals_1.sqlite-wal").exists());
        assert!(!restored.join("auth.json").exists());
        assert!(!restored.join("config.toml").exists());
        assert!(!restored.join("logs_2.sqlite").exists());
        assert!(!restored.join("logs_2.sqlite-wal").exists());
        assert!(!restored.join("state_5.sqlite").exists());
        assert!(!restored.join("goals_1.sqlite-wal").exists());
    }

    #[test]
    fn codex_profile_sync_persists_project_trust_without_copying_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");
        let runtime = temp.path().join("run").join(".codex");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(
            runtime.join("config.toml"),
            r#"
secret_config = true

[projects."/docker-fs"]
trust_level = "trusted"

[projects."/tmp/untrusted"]
trust_level = "untrusted"

[projects."/tmp/ignored"]
other_setting = "ignored"
"#,
        )
        .expect("config");

        sync_runtime_state_to_profile(&profile, &runtime).expect("sync out");
        assert!(!profile.state_dir.join("config.toml").exists());

        let trust = fs::read_to_string(profile.state_dir.join(CODEX_PROJECT_TRUST_FILE))
            .expect("trusted projects");
        assert!(trust.contains("[projects.\"/docker-fs\"]"));
        assert!(trust.contains("trust_level = \"trusted\""));
        assert!(trust.contains("[projects.\"/tmp/untrusted\"]"));
        assert!(!trust.contains("secret_config"));
        assert!(!trust.contains("other_setting"));

        let restored = temp.path().join("restored").join(".codex");
        sync_profile_state_to_runtime(&profile, &restored).expect("sync in");
        write_codex_config(
            &restored,
            "http://127.0.0.1:8080/v1",
            Some("gpt-5.5"),
            None,
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(restored.join("config.toml")).expect("config");
        assert!(config.contains("[projects.\"/docker-fs\"]"));
        assert!(config.contains("trust_level = \"trusted\""));
        assert!(config.contains("[projects.\"/tmp/untrusted\"]"));
        assert!(!config.contains("secret_config"));
        assert!(!config.contains("other_setting"));
    }

    #[test]
    fn claude_profile_sync_preserves_state_and_excludes_generated_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Claude).expect("profile");
        let runtime = temp.path().join("run").join(".claude");
        fs::create_dir_all(runtime.join("projects").join("workspace")).expect("projects");
        fs::create_dir_all(runtime.join("todos")).expect("todos");
        fs::write(runtime.join("history.jsonl"), "{}\n").expect("history");
        fs::write(
            runtime
                .join("projects")
                .join("workspace")
                .join("session.jsonl"),
            "{}\n",
        )
        .expect("session");
        fs::write(runtime.join("todos").join("todo.json"), "{}").expect("todo");
        fs::write(runtime.join("settings.json"), "{\"secret\":true}").expect("settings");
        fs::write(runtime.join(".claude.json"), "{\"secret\":true}").expect("user config");
        fs::write(runtime.join("env"), "ANTHROPIC_API_KEY=secret").expect("env");

        sync_runtime_state_to_profile(&profile, &runtime).expect("sync out");
        let restored = temp.path().join("restored").join(".claude");
        sync_profile_state_to_runtime(&profile, &restored).expect("sync in");

        assert_eq!(
            fs::read_to_string(restored.join("history.jsonl")).expect("history"),
            "{}\n"
        );
        assert_eq!(
            fs::read_to_string(
                restored
                    .join("projects")
                    .join("workspace")
                    .join("session.jsonl")
            )
            .expect("session"),
            "{}\n"
        );
        assert_eq!(
            fs::read_to_string(restored.join("todos").join("todo.json")).expect("todo"),
            "{}"
        );
        assert!(!profile.state_dir.join("settings.json").exists());
        assert!(!profile.state_dir.join(".claude.json").exists());
        assert!(!profile.state_dir.join("env").exists());
        assert!(!restored.join("settings.json").exists());
        assert!(!restored.join(".claude.json").exists());
        assert!(!restored.join("env").exists());
    }

    #[test]
    fn write_codex_config_uses_durable_dot_codex_template() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".codex");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("config.toml"),
            r#"
model = "user-model"
openai_base_url = "http://stale.example/v1"
preferred_auth_method = "chatgpt"
custom_user_setting = "keep"

[features]
responses_websockets = true

[model_providers.custom]
name = "Custom"
base_url = "http://custom.example/v1"

[mcp_servers.proj_creator_dev]
url = "https://create-dev.matthid.de/api/mcp"
bearer_token_env_var = "PROJ_CREATOR_DEV_TOOL_TOKEN"
"#,
        )
        .expect("write template");

        let codex_home = temp.path().join("run").join(".codex");
        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            Some(&config_dir),
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(config.contains("model = \"user-model\""));
        assert!(config.contains("custom_user_setting = \"keep\""));
        assert!(config.contains("responses_websockets = true"));
        assert!(config.contains("[model_providers.custom]"));
        assert!(config.contains("[mcp_servers.proj_creator_dev]"));
        assert!(config.contains("bearer_token_env_var = \"PROJ_CREATOR_DEV_TOOL_TOKEN\""));
        assert!(!config.contains("openai_base_url"));
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("env_key = \"OPENAI_API_KEY\""));
        assert!(config.contains("preferred_auth_method = \"apikey\""));
        assert!(!config.contains("http://stale.example"));
        assert!(!config.contains("preferred_auth_method = \"chatgpt\""));
    }

    #[test]
    fn write_codex_config_merges_global_template_profile_overlay_and_library_mcp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".codex");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("config.toml"),
            r#"
custom_user_setting = "global"

[features]
responses_websockets = true

[mcp_servers.global_docs]
url = "https://global.example/mcp"

[mcp_servers.shared]
command = "node"
args = ["global.js"]
"#,
        )
        .expect("write global template");

        let profile = resolve_agent_profile(&config_dir, "real-estate", ResolvedAgent::Codex)
            .expect("profile");
        fs::create_dir_all(&profile.root_dir).expect("profile dir");
        assert_eq!(
            codex_profile_config_path(&profile),
            profile.root_dir.join("config.toml")
        );
        fs::write(
            codex_profile_config_path(&profile),
            r#"
custom_user_setting = "profile"
custom_profile_setting = "keep"

[mcp_servers.real_estate]
command = "npx"
args = ["-y", "@real-estate/mcp"]

[mcp_servers.shared]
command = "python"
args = ["profile.py"]
"#,
        )
        .expect("write profile config");

        let codex_home = temp.path().join("run").join(".codex");
        write_codex_config_with_profile_and_mcp(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            Some(&config_dir),
            false,
            CodexProviderAuth::EnvKey,
            CodexConfigExtras {
                profile: Some(&profile),
                mcp_servers: &[
                    AgentMcpServer::stdio("library_tools", "node").with_arg("library.js")
                ],
            },
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        assert_eq!(parsed["custom_user_setting"].as_str(), Some("profile"));
        assert_eq!(parsed["custom_profile_setting"].as_str(), Some("keep"));
        assert_eq!(
            parsed["mcp_servers"]["global_docs"]["url"].as_str(),
            Some("https://global.example/mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["real_estate"]["command"].as_str(),
            Some("npx")
        );
        assert_eq!(
            parsed["mcp_servers"]["shared"]["command"].as_str(),
            Some("python")
        );
        let shared_args = parsed["mcp_servers"]["shared"]["args"]
            .as_array()
            .expect("shared args");
        assert_eq!(
            shared_args,
            &vec![toml::Value::String("profile.py".to_string())]
        );
        assert_eq!(
            parsed["mcp_servers"]["library_tools"]["command"].as_str(),
            Some("node")
        );
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
    }

    #[test]
    fn write_codex_config_runtime_model_overrides_template_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".codex");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("config.toml"),
            r#"
model = "user-model"
custom_user_setting = "keep"
"#,
        )
        .expect("write template");

        let codex_home = temp.path().join("run").join(".codex");
        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            Some("kimi/anthropic/kimi-for-coding"),
            Some(&config_dir),
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        assert_eq!(
            parsed["model"].as_str(),
            Some("kimi/anthropic/kimi-for-coding")
        );
        assert_eq!(parsed["custom_user_setting"].as_str(), Some("keep"));
        assert!(parsed["model_catalog_json"].as_str().is_some());
        assert!(!config.contains("model = \"user-model\""));
    }

    #[test]
    fn write_codex_config_generates_model_catalog_for_custom_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            Some("kimi/anthropic/kimi-for-coding"),
            None,
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        let catalog_path = parsed["model_catalog_json"]
            .as_str()
            .expect("model catalog path");
        assert!(Path::new(catalog_path).is_absolute());
        assert_eq!(
            Path::new(catalog_path),
            codex_home.join("model-catalog.json")
        );

        let catalog = fs::read_to_string(catalog_path).expect("read model catalog");
        let catalog: serde_json::Value = serde_json::from_str(&catalog).expect("catalog json");
        let models = catalog["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model["slug"], "kimi/anthropic/kimi-for-coding");
        assert_eq!(model["display_name"], "K2.7 Code");
        assert_eq!(model["supported_in_api"], true);
        assert_eq!(model["truncation_policy"]["mode"], "tokens");
        assert_eq!(model["context_window"], 262144);
    }

    #[test]
    fn write_codex_config_does_not_generate_model_catalog_for_native_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            Some("gpt-5.5"),
            None,
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(!config.contains("model_catalog_json"));
        assert!(!codex_home.join("model-catalog.json").exists());
    }

    #[test]
    fn write_codex_config_preserves_template_model_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".codex");
        fs::create_dir_all(&template_dir).expect("template dir");
        let user_catalog = temp.path().join("custom-codex-models.json");
        let user_catalog_path = user_catalog.to_string_lossy().to_string();
        fs::write(
            template_dir.join("config.toml"),
            format!("model_catalog_json = \"{user_catalog_path}\"\n"),
        )
        .expect("write template");

        let codex_home = temp.path().join("codex");
        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            Some("kimi/anthropic/kimi-for-coding"),
            Some(&config_dir),
            false,
            CodexProviderAuth::EnvKey,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        assert_eq!(
            parsed["model_catalog_json"].as_str(),
            Some(user_catalog_path.as_str())
        );
        assert!(!codex_home.join("model-catalog.json").exists());
    }

    #[test]
    fn write_claude_settings_uses_durable_dot_claude_template() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".claude");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("settings.json"),
            r#"{"customUserSetting":"keep","env":{"ANTHROPIC_BASE_URL":"http://stale.example"}}"#,
        )
        .expect("write template");

        let claude_dir = temp.path().join("run").join(".claude");
        fs::create_dir_all(&claude_dir).expect("claude dir");
        write_claude_settings(
            &claude_dir,
            "http://127.0.0.1:1234",
            Some("runtime-model"),
            Some(&config_dir),
            false,
        )
        .expect("write settings");

        let settings = fs::read_to_string(claude_dir.join("settings.json")).expect("settings");
        assert!(settings.contains("\"customUserSetting\": \"keep\""));
        assert!(settings.contains("\"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:1234\""));
        assert!(!settings.contains("http://stale.example"));
        assert!(settings.contains("\"ANTHROPIC_DEFAULT_SONNET_MODEL\": \"runtime-model\""));
    }

    #[test]
    fn write_codex_config_merges_library_mcp_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");
        let mcp_servers = vec![
            AgentMcpServer::streamable_http(
                "proj_creator_dev",
                "https://create-dev.matthid.de/api/mcp",
            )
            .with_bearer_token_env_var("PROJ_CREATOR_DEV_TOOL_TOKEN"),
            AgentMcpServer::stdio("local_tools", "node")
                .with_arg("server.js")
                .with_arg("--flag")
                .with_env("TOKEN_ENV", "abc"),
        ];

        write_codex_config_with_mcp(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            None,
            false,
            CodexProviderAuth::EnvKey,
            &mcp_servers,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(config.contains("[mcp_servers.proj_creator_dev]"));
        assert!(config.contains("url = \"https://create-dev.matthid.de/api/mcp\""));
        assert!(config.contains("bearer_token_env_var = \"PROJ_CREATOR_DEV_TOOL_TOKEN\""));
        assert!(config.contains("[mcp_servers.local_tools]"));
        assert!(config.contains("command = \"node\""));
        assert!(config.contains("[mcp_servers.local_tools.env]"));
        assert!(config.contains("TOKEN_ENV = \"abc\""));
        let parsed = config.parse::<toml::Value>().expect("parse config");
        let args = parsed["mcp_servers"]["local_tools"]["args"]
            .as_array()
            .expect("local tools args");
        assert_eq!(
            args,
            &vec![
                toml::Value::String("server.js".to_string()),
                toml::Value::String("--flag".to_string())
            ]
        );
    }

    #[test]
    fn write_claude_user_config_copies_template_and_merges_library_mcp_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".claude");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"existing":{"type":"http","url":"https://existing.example/mcp"}}}"#,
        )
        .expect("write template");
        let claude_dir = temp.path().join("run").join(".claude");
        let mcp_servers = vec![AgentMcpServer::streamable_http(
            "proj_creator_dev",
            "https://create-dev.matthid.de/api/mcp",
        )
        .with_bearer_token_env_var("PROJ_CREATOR_DEV_TOOL_TOKEN")];

        write_claude_user_config_with_mcp(&claude_dir, Some(&config_dir), &mcp_servers)
            .expect("write user config");

        let settings = fs::read_to_string(claude_dir.join(".claude.json")).expect("user config");
        assert!(settings.contains("\"theme\": \"dark\""));
        assert!(settings.contains("\"existing\""));
        assert!(settings.contains("\"proj_creator_dev\""));
        assert!(settings.contains("\"type\": \"http\""));
        assert!(settings.contains("\"url\": \"https://create-dev.matthid.de/api/mcp\""));
        assert!(settings.contains("\"Authorization\": \"Bearer ${PROJ_CREATOR_DEV_TOOL_TOKEN}\""));
    }

    #[test]
    fn write_codex_config_uses_chatgpt_auth_in_subscription_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            None,
            false,
            CodexProviderAuth::OpenAiAuth,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(!config.contains("openai_base_url"));
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("preferred_auth_method = \"chatgpt\""));
        assert!(config.contains("requires_openai_auth = true"));
        assert!(!config.contains("env_key = \"OPENAI_API_KEY\""));
    }

    #[test]
    fn write_codex_config_overwrites_template_auth_method_for_subscription_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".codex");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("config.toml"),
            r#"
openai_base_url = "http://stale.example/v1"
preferred_auth_method = "apikey"
"#,
        )
        .expect("write template");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            Some(&config_dir),
            false,
            CodexProviderAuth::OpenAiAuth,
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(!config.contains("openai_base_url"));
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("preferred_auth_method = \"chatgpt\""));
        assert!(config.contains("requires_openai_auth = true"));
        assert!(!config.contains("env_key = \"OPENAI_API_KEY\""));
        assert!(!config.contains("http://stale.example"));
        assert!(!config.contains("preferred_auth_method = \"apikey\""));
    }

    #[test]
    fn write_codex_config_uses_command_backed_auth_for_auth_pool_proxy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            None,
            false,
            CodexProviderAuth::Command {
                command: "/tmp/ai-fence-cli".to_string(),
                args: vec![
                    "codex-auth-token".to_string(),
                    "--auth-json".to_string(),
                    "/tmp/codex/auth.json".to_string(),
                ],
            },
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("[model_providers.ai_fence.auth]"));
        assert!(config.contains("command = \"/tmp/ai-fence-cli\""));
        assert!(config.contains("\"codex-auth-token\""));
        assert!(!config.contains("openai_base_url"));
        assert!(!config.contains("env_key = \"OPENAI_API_KEY\""));
        assert!(!config.contains("requires_openai_auth = true"));
    }

    #[test]
    fn write_codex_config_uses_env_bearer_for_subscription_proxy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");

        write_codex_config(
            &codex_home,
            "http://127.0.0.1:1234",
            None,
            None,
            false,
            CodexProviderAuth::EnvBearer {
                env_key: "CUSTOM_PROVIDER_TOKEN".to_string(),
            },
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        assert!(config.contains("model_provider = \"ai_fence\""));
        assert!(config.contains("[model_providers.ai_fence]"));
        assert!(config.contains("base_url = \"http://127.0.0.1:1234/v1\""));
        assert!(config.contains("env_key = \"CUSTOM_PROVIDER_TOKEN\""));
        assert!(!config.contains("openai_base_url"));
        assert!(!config.contains("requires_openai_auth = true"));
    }
}
