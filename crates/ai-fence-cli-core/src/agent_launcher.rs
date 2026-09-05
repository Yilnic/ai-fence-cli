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
    Interpreter,
    Junie,
    Pi,
    Dsh,
    Kimi,
    Copilot,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedAgent {
    Codex,
    Claude,
    Interpreter,
    Junie,
    Pi,
    Dsh,
    Kimi,
    Copilot,
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
    /// Ordered models to expose through Codex's native model picker.
    pub catalog_models: &'a [String],
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexModelSelection<'a> {
    pub explicit_model: Option<&'a str>,
    pub default_model: Option<&'a str>,
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

    pub(crate) fn to_codex_toml(&self) -> Result<TomlTable> {
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
            Self::Interpreter => "open interpreter",
            Self::Junie => "junie",
            Self::Pi => "pi",
            Self::Dsh => "dsh",
            Self::Kimi => "kimi",
            Self::Copilot => "copilot",
            Self::Generic => "generic command",
        }
    }
}

pub fn resolve_launch(agent: AgentKind, command: &[String]) -> Result<LaunchSpec> {
    let resolved = match agent {
        AgentKind::Auto => detect_agent(command).unwrap_or(ResolvedAgent::Generic),
        AgentKind::Codex => ResolvedAgent::Codex,
        AgentKind::Claude => ResolvedAgent::Claude,
        AgentKind::Interpreter => ResolvedAgent::Interpreter,
        AgentKind::Junie => ResolvedAgent::Junie,
        AgentKind::Pi => ResolvedAgent::Pi,
        AgentKind::Dsh => ResolvedAgent::Dsh,
        AgentKind::Kimi => ResolvedAgent::Kimi,
        AgentKind::Copilot => ResolvedAgent::Copilot,
        AgentKind::Generic => ResolvedAgent::Generic,
    };
    let mut command = if command.is_empty() {
        default_command(resolved)?
    } else {
        command.to_vec()
    };
    // If the user passed args without the agent binary (e.g. "exec" instead of "codex exec"),
    // prepend the default binary so the command is always runnable.
    let expected_binary = default_agent_binary(resolved);
    if !expected_binary.is_empty() {
        let first = command.first().and_then(|s| Path::new(s).file_name());
        let first_name = first.and_then(|s| s.to_str());
        if !is_agent_binary(resolved, first_name) {
            command.insert(0, expected_binary.to_string());
        }
    }
    Ok(LaunchSpec {
        agent: resolved,
        command,
    })
}

fn default_agent_binary(agent: ResolvedAgent) -> &'static str {
    match agent {
        ResolvedAgent::Codex => "codex",
        ResolvedAgent::Claude => "claude",
        ResolvedAgent::Interpreter => "interpreter",
        ResolvedAgent::Junie => "junie",
        ResolvedAgent::Pi => "pi",
        ResolvedAgent::Dsh => "dsh",
        ResolvedAgent::Kimi => "kimi",
        ResolvedAgent::Copilot => "copilot",
        ResolvedAgent::Generic => "",
    }
}

fn is_agent_binary(agent: ResolvedAgent, binary: Option<&str>) -> bool {
    matches!(
        (agent, binary),
        (ResolvedAgent::Codex, Some("codex"))
            | (ResolvedAgent::Claude, Some("claude" | "claude-code"))
            | (ResolvedAgent::Interpreter, Some("interpreter" | "i"))
            | (ResolvedAgent::Junie, Some("junie"))
            | (ResolvedAgent::Pi, Some("pi"))
            | (ResolvedAgent::Dsh, Some("dsh"))
            | (ResolvedAgent::Kimi, Some("kimi"))
            | (ResolvedAgent::Copilot, Some("copilot"))
    )
}

pub fn default_command(agent: ResolvedAgent) -> Result<Vec<String>> {
    match agent {
        ResolvedAgent::Codex => Ok(vec!["codex".to_string()]),
        ResolvedAgent::Claude => Ok(vec!["claude".to_string()]),
        ResolvedAgent::Interpreter => Ok(vec!["interpreter".to_string()]),
        ResolvedAgent::Junie => Ok(vec!["junie".to_string()]),
        ResolvedAgent::Pi => Ok(vec!["pi".to_string()]),
        ResolvedAgent::Dsh => Ok(vec!["dsh".to_string()]),
        ResolvedAgent::Kimi => Ok(vec!["kimi".to_string()]),
        ResolvedAgent::Copilot => Ok(vec!["copilot".to_string()]),
        ResolvedAgent::Generic => {
            anyhow::bail!(
                "ai-fence-cli run requires --agent codex/claude/interpreter/junie/pi/dsh/kimi/copilot or a command after --"
            )
        }
    }
}

pub fn detect_agent(command: &[String]) -> Option<ResolvedAgent> {
    let first = command.first()?;
    let name = Path::new(first).file_name()?.to_str()?;
    match name {
        "codex" => Some(ResolvedAgent::Codex),
        "claude" | "claude-code" => Some(ResolvedAgent::Claude),
        "interpreter" | "i" => Some(ResolvedAgent::Interpreter),
        "junie" => Some(ResolvedAgent::Junie),
        "pi" => Some(ResolvedAgent::Pi),
        "dsh" => Some(ResolvedAgent::Dsh),
        "kimi" => Some(ResolvedAgent::Kimi),
        "copilot" | "copilot-cli" => Some(ResolvedAgent::Copilot),
        _ => Some(ResolvedAgent::Generic),
    }
}

pub fn default_ai_fence_home() -> Option<PathBuf> {
    std::env::var_os("AI_FENCE_HOME")
        .map(PathBuf::from)
        .or_else(|| effective_user_home_dir().map(|home| home.join(".ai-fence")))
}

/// Resolve the inherited home, or fall back to the effective operating-system
/// user's passwd entry. Privileged launchers can intentionally clear `HOME`,
/// and using the effective uid avoids assigning every such launch to `/root`.
pub fn effective_user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(passwd_home_for_effective_user)
        .or_else(dirs::home_dir)
}

#[cfg(unix)]
fn passwd_home_for_effective_user() -> Option<PathBuf> {
    use std::ffi::{CStr, OsString};
    use std::os::unix::ffi::OsStringExt;

    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if initial_size > 0 {
        initial_size as usize
    } else {
        16 * 1024
    };
    loop {
        let mut passwd = unsafe { std::mem::zeroed::<libc::passwd>() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut passwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer_size < 1024 * 1024 {
            buffer_size *= 2;
            continue;
        }
        if status != 0 || result.is_null() || passwd.pw_dir.is_null() {
            return None;
        }
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        return (!home.is_empty()).then(|| PathBuf::from(OsString::from_vec(home.to_vec())));
    }
}

#[cfg(not(unix))]
fn passwd_home_for_effective_user() -> Option<PathBuf> {
    None
}

pub fn resolve_config_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    default_ai_fence_home()
        .context("could not determine the effective user's home and AI_FENCE_HOME was not provided")
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
    pub sqlite_dir: PathBuf,
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
        ResolvedAgent::Interpreter => "interpreter",
        ResolvedAgent::Junie => "junie",
        ResolvedAgent::Pi => "pi",
        ResolvedAgent::Dsh => "dsh",
        ResolvedAgent::Kimi => "kimi",
        ResolvedAgent::Copilot => "copilot",
        ResolvedAgent::Generic => {
            anyhow::bail!(
                "managed agent profiles are only supported for Codex, Claude Code, Open Interpreter, Junie, pi, dsh, kimi, and copilot"
            )
        }
    };
    let name = sanitize_profile_name(profile_name)?;
    let root_dir = config_dir.join("profiles").join(&name).join(agent_dir);
    Ok(AgentProfile {
        name,
        agent,
        state_dir: root_dir.join("state"),
        sqlite_dir: root_dir.join("sqlite"),
        lock_path: root_dir.join(".sync.lock"),
        metadata_path: root_dir.join("metadata.json"),
        root_dir,
    })
}

/// Return Codex's durable, profile-scoped SQLite directory.
///
/// Codex supports keeping SQLite-backed indexes outside `CODEX_HOME`. Managed
/// launches use that boundary so auth and generated configuration remain
/// isolated per run while concurrent sessions share Codex's live database and
/// do not rebuild it from every retained rollout at startup.
pub fn prepare_codex_profile_sqlite_home(profile: &AgentProfile) -> Result<PathBuf> {
    if profile.agent != ResolvedAgent::Codex {
        anyhow::bail!(
            "profile '{}' belongs to {}; a Codex SQLite home requires a Codex profile",
            profile.name,
            profile.agent.as_str()
        );
    }
    fs::create_dir_all(&profile.sqlite_dir).with_context(|| {
        format!(
            "failed to create durable Codex SQLite home {}",
            profile.sqlite_dir.display()
        )
    })?;
    Ok(profile.sqlite_dir.clone())
}

/// Return the stable, profile-scoped home that Codex owns across launches.
///
/// AI Fence deliberately does not copy or interpret files inside this
/// directory. Keeping the complete native home at a stable path lets Codex
/// migrate its own SQLite indexes and any future state formats atomically.
pub fn prepare_codex_profile_home(profile: &AgentProfile) -> Result<PathBuf> {
    if profile.agent != ResolvedAgent::Codex {
        anyhow::bail!(
            "profile '{}' belongs to {}; a Codex home requires a Codex profile",
            profile.name,
            profile.agent.as_str()
        );
    }
    fs::create_dir_all(&profile.state_dir).with_context(|| {
        format!(
            "failed to create durable Codex home {}",
            profile.state_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&profile.state_dir, fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "failed to secure durable Codex home {}",
                    profile.state_dir.display()
                )
            },
        )?;
    }
    Ok(profile.state_dir.clone())
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

#[cfg(all(unix, not(target_os = "linux")))]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 performs permission and existence checks without sending a signal.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
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
    if shares_live_session_state(profile.agent) {
        fs::create_dir_all(profile.state_dir.join("sessions")).with_context(|| {
            format!(
                "failed to create durable session storage {}",
                profile.state_dir.join("sessions").display()
            )
        })?;
    }
    sync_agent_state(profile.agent, &profile.state_dir, runtime_agent_dir, true)
}

pub fn sync_runtime_state_to_profile(
    profile: &AgentProfile,
    runtime_agent_dir: &Path,
) -> Result<()> {
    let _lock = acquire_agent_profile_lock(profile)?;
    if profile.agent == ResolvedAgent::Codex {
        remove_codex_sqlite_profile_state(&profile.state_dir)?;
    }
    sync_agent_state(profile.agent, runtime_agent_dir, &profile.state_dir, false)?;
    if profile.agent == ResolvedAgent::Codex {
        persist_codex_profile_selection(runtime_agent_dir, profile)?;
        persist_codex_project_trust(runtime_agent_dir, &profile.state_dir)?;
    }
    Ok(())
}

fn sync_agent_state(
    agent: ResolvedAgent,
    source_dir: &Path,
    target_dir: &Path,
    link_durable_sessions: bool,
) -> Result<()> {
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
            let source = entry.path();
            let target = target_dir.join(file_name.as_ref());
            if link_durable_sessions && file_name == "sessions" && source.is_dir() {
                link_durable_session_dir(&source, &target)?;
            } else {
                copy_profile_path(&source, &target)?;
            }
        }
    }
    Ok(())
}

fn shares_live_session_state(agent: ResolvedAgent) -> bool {
    matches!(agent, ResolvedAgent::Codex | ResolvedAgent::Interpreter)
}

fn link_durable_session_dir(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        return copy_profile_path(source, target);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", source.display()))?;

    #[cfg(unix)]
    let link_result = std::os::unix::fs::symlink(&source, target);
    #[cfg(windows)]
    let link_result = std::os::windows::fs::symlink_dir(&source, target);
    #[cfg(not(any(unix, windows)))]
    let link_result = Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "directory links are unsupported on this platform",
    ));

    match link_result {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!(
                source = %source.display(),
                target = %target.display(),
                error = %error,
                "could not link durable session storage; falling back to a copy"
            );
            copy_profile_path(&source, target)
        }
    }
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
        // Junie keeps its home mostly managed by AI Fence; persist only
        // resumable session/history artifacts and never config or auth state.
        ResolvedAgent::Junie => {
            matches!(name, "history.jsonl" | "sessions")
        }
        // pi keeps its agent dir mostly managed by AI Fence (models.json is
        // regenerated per run); persist only resumable session artifacts.
        ResolvedAgent::Pi => {
            matches!(name, "sessions")
        }
        // dsh keeps its home mostly managed by AI Fence (settings.yaml is
        // regenerated per run); persist only resumable session artifacts.
        ResolvedAgent::Dsh => {
            matches!(name, "sessions")
        }
        // kimi keeps its share dir mostly managed by AI Fence (config.toml is
        // regenerated per run); persist only resumable session artifacts.
        ResolvedAgent::Kimi => {
            matches!(name, "sessions")
        }
        // copilot keeps its home mostly managed by AI Fence; persist only
        // resumable session artifacts and never config or auth state.
        ResolvedAgent::Copilot => {
            matches!(name, "sessions")
        }
        // Open Interpreter's current runtime is Codex-derived, but its
        // state/credential format is intentionally independent. Persist only
        // resumable session/history artifacts and never config or auth state.
        ResolvedAgent::Interpreter => {
            matches!(name, "history.jsonl" | "sessions" | "shell_snapshots")
        }
        ResolvedAgent::Generic => false,
    }
}

fn persist_codex_profile_selection(
    runtime_codex_home: &Path,
    profile: &AgentProfile,
) -> Result<()> {
    let runtime_config_path = runtime_codex_home.join("config.toml");
    if !runtime_config_path.is_file() {
        return Ok(());
    }
    let runtime_data = fs::read_to_string(&runtime_config_path)
        .with_context(|| format!("failed to read {}", runtime_config_path.display()))?;
    let runtime_config = runtime_data
        .parse::<toml::Value>()
        .with_context(|| format!("failed to parse {}", runtime_config_path.display()))?;

    let mut profile_config = read_codex_profile_config_template(Some(profile))?.unwrap_or_default();
    let mut changed = false;
    for key in ["model", "model_reasoning_effort"] {
        let Some(value) = runtime_config
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let value = toml::Value::String(value.to_string());
        if profile_config.get(key) != Some(&value) {
            profile_config.insert(key.to_string(), value);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }

    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let target = codex_profile_config_path(profile);
    fs::write(
        &target,
        toml::to_string_pretty(&toml::Value::Table(profile_config))
            .context("failed to render Codex profile selection")?,
    )
    .with_context(|| format!("failed to write {}", target.display()))
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
            catalog_models: &[],
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
            catalog_models: &[],
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
    write_codex_config_with_model_selection(
        codex_home,
        proxy_base,
        CodexModelSelection {
            explicit_model: model,
            default_model: None,
        },
        template_dir,
        yolo,
        provider_auth,
        extras,
    )
    .map(|_| ())
}

pub fn write_codex_config_with_profile_and_default_model(
    codex_home: &Path,
    proxy_base: &str,
    model: CodexModelSelection<'_>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
    profile: Option<&AgentProfile>,
) -> Result<Option<String>> {
    write_codex_config_with_model_selection(
        codex_home,
        proxy_base,
        model,
        template_dir,
        yolo,
        provider_auth,
        CodexConfigExtras {
            profile,
            mcp_servers: &[],
            catalog_models: &[],
        },
    )
}

/// Write a managed Codex configuration using an explicit/default model
/// selection and optional profile, MCP, and ordered model-catalog extras.
pub fn write_codex_config_with_model_selection(
    codex_home: &Path,
    proxy_base: &str,
    model: CodexModelSelection<'_>,
    template_dir: Option<&Path>,
    yolo: bool,
    provider_auth: CodexProviderAuth,
    extras: CodexConfigExtras<'_>,
) -> Result<Option<String>> {
    fs::create_dir_all(codex_home)
        .with_context(|| format!("failed to create CODEX_HOME {}", codex_home.display()))?;

    let mut config = read_codex_config_template(template_dir)?.unwrap_or_default();
    if let Some(profile_config) = read_codex_profile_config_template(extras.profile)? {
        merge_codex_config_overlay(&mut config, profile_config);
    }
    if extras.profile.is_some() {
        // The entire managed profile is now the stable native CODEX_HOME.
        // Remove stale template overrides so Codex keeps every database beside
        // the rollout paths it records and owns its future migrations.
        config.remove("sqlite_home");
    }

    insert_string_if_missing(&mut config, "cli_auth_credentials_store", "file");
    let features = ensure_table(&mut config, "features");
    features
        .entry("responses_websockets".to_string())
        .or_insert(toml::Value::Boolean(false));

    if let Some(m) = model.explicit_model {
        insert_string(&mut config, "model", m);
    } else if let Some(m) = model.default_model {
        insert_string_if_missing(&mut config, "model", m);
    }
    let effective_model = config
        .get("model")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let uses_native_subscription_catalog = matches!(&provider_auth, CodexProviderAuth::OpenAiAuth);
    if uses_native_subscription_catalog {
        // Subscription-backed sessions must discover models and reasoning
        // capabilities from OpenAI. A template-local catalog can expose
        // direct-provider aliases on the wrong auth lane and replace native
        // reasoning levels with synthetic metadata.
        config.remove("model_catalog_json");
    } else if !config.contains_key("model_catalog_json") {
        let mut catalog_models = extras.catalog_models.to_vec();
        if let Some(effective_model) = effective_model.as_deref() {
            if !catalog_models.iter().any(|model| model == effective_model) {
                catalog_models.insert(0, effective_model.to_string());
            }
        }
        let should_write_catalog = !catalog_models.is_empty()
            && (!extras.catalog_models.is_empty()
                || catalog_models
                    .first()
                    .is_some_and(|model| should_write_codex_model_catalog(model)));
        if should_write_catalog {
            let catalog_path = codex_home.join("model-catalog.json");
            write_ai_fence_model_catalog(&catalog_path, &catalog_models)?;
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
    })?;
    Ok(effective_model)
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

/// Write the Codex/Open Interpreter model catalog shared by managed agents.
/// Duplicate and empty model ids are ignored while preserving selection order.
pub fn write_ai_fence_model_catalog(path: &Path, models: &[String]) -> Result<()> {
    let mut unique_models = Vec::new();
    for model in models {
        let model = model.trim();
        if !model.is_empty() && !unique_models.contains(&model) {
            unique_models.push(model);
        }
    }
    if unique_models.is_empty() {
        anyhow::bail!("AI Fence model catalog requires at least one model");
    }
    let catalog = serde_json::json!({
        "models": unique_models
            .into_iter()
            .enumerate()
            .map(|(index, model)| codex_model_catalog_entry(model, index))
            .collect::<Vec<_>>()
    });
    fs::write(
        path,
        serde_json::to_string_pretty(&catalog).context("failed to render Codex model catalog")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

fn codex_model_catalog_entry(model: &str, priority: usize) -> serde_json::Value {
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
        "priority": i64::try_from(priority).unwrap_or(i64::MAX),
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
    write_claude_settings_with_models(claude_dir, proxy_base, model, &[], template_dir, yolo)
}

/// Write Claude Code settings with deterministic Sonnet/Opus/Haiku aliases
/// backed by the user's ordered AI Fence model selection.
pub fn write_claude_settings_with_models(
    claude_dir: &Path,
    proxy_base: &str,
    model: Option<&str>,
    selected_models: &[String],
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
        // Fenced model ids are not in Claude Code's known-model table, so it
        // would otherwise assume a 200k window and compact proactively. Defer
        // to the API instead (reactive mode); overflow is still caught by the
        // fence's own compaction. A value in the durable template wins.
        env_obj
            .entry("CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT".to_string())
            .or_insert_with(|| serde_json::json!("1"));
        let mut alias_models = selected_models
            .iter()
            .map(|model| model.trim())
            .filter(|model| !model.is_empty())
            .collect::<Vec<_>>();
        if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
            if let Some(index) = alias_models.iter().position(|selected| *selected == model) {
                alias_models.remove(index);
            }
            alias_models.insert(0, model);
        }
        if let Some(default_model) = alias_models.first().copied() {
            let sonnet = default_model;
            let opus = alias_models.get(1).copied().unwrap_or(default_model);
            let haiku = alias_models.get(2).copied().unwrap_or(default_model);
            env_obj.insert(
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                serde_json::json!(haiku),
            );
            env_obj.insert(
                "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                serde_json::json!(sonnet),
            );
            env_obj.insert(
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                serde_json::json!(opus),
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

/// Generate one Claude Code custom subagent per selected model. This keeps
/// models beyond Claude's three built-in aliases directly addressable.
pub fn write_claude_model_agents(claude_dir: &Path, selected_models: &[String]) -> Result<()> {
    let models = selected_models
        .iter()
        .map(|model| model.trim())
        .filter(|model| !model.is_empty())
        .collect::<Vec<_>>();
    if models.is_empty() {
        return Ok(());
    }
    let agents_dir = claude_dir.join("agents");
    fs::create_dir_all(&agents_dir)
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;
    for (index, model) in models.into_iter().enumerate() {
        let slug = model
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug = slug.chars().take(48).collect::<String>();
        let name = format!("ai-fence-model-{:02}-{}", index + 1, slug);
        let path = agents_dir.join(format!("{name}.md"));
        let quoted_model =
            serde_json::to_string(model).context("failed to quote Claude model id")?;
        let contents = format!(
            "---\nname: {name}\ndescription: Delegate work to AI Fence model {quoted_model}.\nmodel: {quoted_model}\n---\nUse this model for the delegated task. Follow the parent agent's instructions and report a concise result.\n"
        );
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

pub fn write_claude_user_config(claude_dir: &Path, template_dir: Option<&Path>) -> Result<()> {
    write_claude_user_config_for_profile(claude_dir, template_dir, None)
}

/// Write Claude Code's user-scoped `.claude.json` from the global template plus
/// caller-supplied MCP servers, without a managed profile overlay.
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
    write_rendered_claude_user_config(claude_dir, user_config)
}

/// Write Claude Code's user-scoped `.claude.json`, merging the global template
/// with an optional managed profile overlay before writing the run copy.
pub fn write_claude_user_config_for_profile(
    claude_dir: &Path,
    template_dir: Option<&Path>,
    profile: Option<&AgentProfile>,
) -> Result<()> {
    let mut user_config =
        read_claude_user_config_template(template_dir)?.unwrap_or_else(|| serde_json::json!({}));
    if !user_config.is_object() {
        anyhow::bail!("Claude user config template must be a JSON object");
    }
    if let Some(overlay) = read_claude_profile_user_config_template(profile)? {
        merge_claude_user_config_overlay(&mut user_config, overlay)?;
    }
    write_rendered_claude_user_config(claude_dir, user_config)
}

fn write_rendered_claude_user_config(
    claude_dir: &Path,
    user_config: serde_json::Value,
) -> Result<()> {
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

pub fn claude_profile_user_config_path(profile: &AgentProfile) -> PathBuf {
    profile.root_dir.join(".claude.json")
}

pub fn upsert_claude_profile_mcp_server(
    profile: &AgentProfile,
    server: &AgentMcpServer,
) -> Result<()> {
    if profile.agent != ResolvedAgent::Claude {
        anyhow::bail!("Claude profile MCP config requires a Claude agent profile");
    }
    fs::create_dir_all(&profile.root_dir)
        .with_context(|| format!("failed to create profile {}", profile.root_dir.display()))?;
    let mut user_config = read_claude_profile_user_config_template(Some(profile))?
        .unwrap_or_else(|| serde_json::json!({}));
    if !user_config.is_object() {
        anyhow::bail!("Claude profile user config overlay must be a JSON object");
    }
    merge_claude_mcp_servers(&mut user_config, std::slice::from_ref(server))?;
    let target = claude_profile_user_config_path(profile);
    fs::write(
        &target,
        serde_json::to_string_pretty(&user_config)
            .context("failed to render Claude profile user config")?,
    )
    .with_context(|| format!("failed to write {}", target.display()))
}

fn read_claude_profile_user_config_template(
    profile: Option<&AgentProfile>,
) -> Result<Option<serde_json::Value>> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let Some(template) = read_first_existing_template(&[
        claude_profile_user_config_path(profile),
        profile.root_dir.join("claude-user-config.json"),
    ])?
    else {
        return Ok(None);
    };
    serde_json::from_str(&template)
        .with_context(|| {
            format!(
                "invalid Claude profile user config JSON under {}",
                profile.root_dir.display()
            )
        })
        .map(Some)
}

/// Deep-merge a profile overlay into the global Claude user config. Named
/// `mcpServers` entries replace their global counterparts one by one, matching
/// the Codex and Open Interpreter profile overlay semantics.
fn merge_claude_user_config_overlay(
    user_config: &mut serde_json::Value,
    overlay: serde_json::Value,
) -> Result<()> {
    let overlay = match overlay {
        serde_json::Value::Object(overlay) => overlay,
        _ => anyhow::bail!("Claude profile user config overlay must be a JSON object"),
    };
    for (key, value) in overlay {
        if key == "mcpServers" {
            merge_claude_mcp_server_entries(user_config, value)?;
        } else {
            merge_claude_json_value(user_config, key, value);
        }
    }
    Ok(())
}

fn merge_claude_json_value(
    parent: &mut serde_json::Value,
    key: String,
    overlay: serde_json::Value,
) {
    let entries = match overlay {
        serde_json::Value::Object(entries) => entries,
        overlay => {
            if let Some(obj) = parent.as_object_mut() {
                obj.insert(key, overlay);
            }
            return;
        }
    };
    let base = match parent.get_mut(&key) {
        Some(base) if base.is_object() => base,
        _ => {
            if let Some(obj) = parent.as_object_mut() {
                obj.insert(key, serde_json::Value::Object(entries));
            }
            return;
        }
    };
    for (child_key, child_value) in entries {
        merge_claude_json_value(base, child_key, child_value);
    }
}

fn merge_claude_mcp_server_entries(
    user_config: &mut serde_json::Value,
    overlay: serde_json::Value,
) -> Result<()> {
    let obj = user_config
        .as_object_mut()
        .context("Claude user config must be a JSON object")?;
    let mcp_servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !mcp_servers.is_object() {
        anyhow::bail!("Claude user config mcpServers must be a JSON object");
    }
    let overlay = match overlay {
        serde_json::Value::Object(entries) => entries,
        _ => anyhow::bail!("Claude profile mcpServers overlay must be a JSON object"),
    };
    let mcp_obj = mcp_servers.as_object_mut().expect("object checked above");
    for (name, value) in overlay {
        mcp_obj.insert(name, value);
    }
    Ok(())
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

pub const JUNIE_HOME_ENV_VAR: &str = "JUNIE_HOME";

/// The user-level home Junie resolves when it is not managed by AI Fence.
pub fn default_user_junie_home() -> Option<PathBuf> {
    std::env::var_os(JUNIE_HOME_ENV_VAR)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".junie")))
}

/// Resolve a run-scoped Junie home and refuse the normal user home.
/// The caller must create the returned directory before launching.
pub fn resolve_junie_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let junie_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".junie"));
    if let Some(default_home) = default_user_junie_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&junie_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default JUNIE_HOME {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(junie_home)
}

/// Write the managed Junie home env file pointing the agent at the AI Fence
/// proxy. Junie is launched through a local or backend proxy, so every
/// supported OpenAI/Anthropic-compatible endpoint variable is set here.
pub fn write_junie_settings(
    junie_home: &Path,
    proxy_base: &str,
    api_key: Option<&str>,
) -> Result<()> {
    write_agent_env_file(junie_home, proxy_base, api_key)
}

/// Transport API declared for a Junie BYOK custom model file. Values match
/// the `apiType` enum in Junie's custom model schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JunieApiType {
    OpenAiCompletion,
    OpenAiResponses,
    Anthropic,
}

impl JunieApiType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletion => "OpenAICompletion",
            Self::OpenAiResponses => "OpenAIResponses",
            Self::Anthropic => "Anthropic",
        }
    }
}

/// One custom Junie BYOK model entry written into `<JUNIE_HOME>/models`.
/// Junie discovers these files through its default model-location directory
/// and selects them on the command line as `custom:<file stem>`, so each
/// entry carries the full endpoint URL instead of relying on base-url
/// environment variables (which Junie does not support).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JunieModelConfig {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    pub api_type: JunieApiType,
}

/// Sanitize a fence route id into a Junie model file stem. Slashes and any
/// other characters outside `[A-Za-z0-9._-]` become dashes so the derived
/// `custom:<stem>` selector stays a single valid path component.
pub fn junie_model_stem(id: &str) -> String {
    let sanitized: String = id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "fenced-model".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Write Junie BYOK model files pointing at the AI Fence proxy. Junie has no
/// base-url environment support, so every fence route is declared as a custom
/// model file under `<JUNIE_HOME>/models`; the key is referenced through the
/// whole-value `${OPENAI_API_KEY}` environment placeholder that the launcher
/// exports to the agent process.
pub fn write_junie_models(junie_home: &Path, models: &[JunieModelConfig]) -> Result<()> {
    let models_dir = junie_home.join("models");
    fs::create_dir_all(&models_dir)
        .with_context(|| format!("failed to create {}", models_dir.display()))?;
    for model in models {
        if model.id.trim().is_empty() {
            anyhow::bail!("junie model id must not be empty");
        }
        let stem = junie_model_stem(&model.id);
        let payload = serde_json::json!({
            "id": model.id,
            "displayName": model.display_name,
            "baseUrl": model.base_url,
            "apiKey": "${OPENAI_API_KEY}",
            "apiType": model.api_type.as_str(),
        });
        fs::write(models_dir.join(format!("{stem}.json")), payload.to_string()).with_context(
            || {
                format!(
                    "failed to write {}",
                    models_dir.join(format!("{stem}.json")).display()
                )
            },
        )?;
    }
    Ok(())
}

pub const PI_HOME_ENV_VAR: &str = "PI_CODING_AGENT_DIR";

/// The user-level agent dir pi resolves when it is not managed by AI Fence.
pub fn default_user_pi_home() -> Option<PathBuf> {
    std::env::var_os(PI_HOME_ENV_VAR)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi").join("agent"))
        })
}

/// Resolve a run-scoped pi agent dir and refuse the normal user home.
/// The caller must create the returned directory before launching.
pub fn resolve_pi_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let pi_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".pi").join("agent"));
    if let Some(default_home) = default_user_pi_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&pi_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default PI_CODING_AGENT_DIR {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(pi_home)
}

/// Transport API declared for a pi custom provider. Values match the
/// `api` strings documented in pi's models.json provider configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiProviderApi {
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

impl PiProviderApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }
}

/// One custom pi provider entry for the managed models.json. The caller
/// derives base URLs and transports from the selected model/catalog because
/// pi has no base-url environment variable support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api: PiProviderApi,
    pub models: Vec<String>,
}

/// Write the managed pi agent dir configuration pointing the agent at the AI
/// Fence proxy. pi ignores OPENAI_BASE_URL/ANTHROPIC_BASE_URL, so the proxy
/// is configured through custom providers in models.json instead; the env
/// file is still written for parity with the other managed agents.
pub fn write_pi_settings(
    pi_home: &Path,
    proxy_base: &str,
    api_key: Option<&str>,
    providers: &[PiProviderConfig],
    default_model: Option<&str>,
) -> Result<()> {
    write_agent_env_file(pi_home, proxy_base, api_key)?;

    // Pin the managed default model so bare `pi` launches cannot fall back to
    // a built-in provider picked up through ambient provider credentials.
    if let Some(default_model) = default_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        if let Some(provider) = providers
            .iter()
            .find(|provider| provider.models.iter().any(|model| model == default_model))
        {
            let settings = serde_json::json!({
                "defaultProvider": provider.name,
                "defaultModel": default_model,
            });
            fs::write(
                pi_home.join("settings.json"),
                serde_json::to_string_pretty(&settings)?,
            )
            .with_context(|| {
                format!(
                    "failed to write {}",
                    pi_home.join("settings.json").display()
                )
            })?;
        }
    }

    let mut provider_json = serde_json::Map::new();
    for provider in providers {
        if provider.name.trim().is_empty() {
            anyhow::bail!("pi provider name must not be empty");
        }
        let mut entry = serde_json::Map::new();
        entry.insert("baseUrl".to_string(), serde_json::json!(provider.base_url));
        entry.insert("api".to_string(), serde_json::json!(provider.api.as_str()));
        if let Some(api_key) = api_key {
            entry.insert("apiKey".to_string(), serde_json::json!(api_key));
        }
        let models: Vec<serde_json::Value> = provider
            .models
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();
        entry.insert("models".to_string(), serde_json::json!(models));
        provider_json.insert(provider.name.clone(), serde_json::Value::Object(entry));
    }
    let models_json = serde_json::json!({ "providers": serde_json::Value::Object(provider_json) });
    fs::write(
        pi_home.join("models.json"),
        serde_json::to_string_pretty(&models_json)?,
    )
    .with_context(|| format!("failed to write {}", pi_home.join("models.json").display()))
}

pub const DSH_HOME_ENV_VAR: &str = "DSH_HOME";

/// Environment variable referenced by the managed provider routes'
/// `apiKeyEnv` entries and exported by the launcher to the dsh process.
pub const DSH_API_KEY_ENV_VAR: &str = "AI_FENCE_DSH_API_KEY";

/// The user-level harness home dsh resolves when it is not managed by AI Fence.
pub fn default_user_dsh_home() -> Option<PathBuf> {
    std::env::var_os(DSH_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".dsh")))
}

/// Resolve a run-scoped dsh home and refuse the normal user home.
/// The caller must create the returned directory before launching.
pub fn resolve_dsh_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let dsh_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".dsh"));
    if let Some(default_home) = default_user_dsh_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&dsh_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default DSH_HOME {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(dsh_home)
}

/// Emit a single-quoted YAML scalar. Catalog model ids and gateway URLs are
/// plain enough today, but quoting keeps any odd character from turning into
/// YAML structure.
fn yaml_scalar(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Write the managed dsh home configuration pointing the harness at the AI
/// Fence proxy. The DeepSeek Harness resolves providers through the same
/// pi-ai layer as pi itself, so the fence routes are declared as custom
/// `llm-pi-ai` providers in `$DSH_HOME/settings.yaml`; credentials are never
/// embedded in the file but referenced through `apiKeyEnv` and exported by
/// the launcher.
pub fn write_dsh_settings(
    dsh_home: &Path,
    proxy_base: &str,
    api_key: Option<&str>,
    providers: &[PiProviderConfig],
    default_model: Option<&str>,
) -> Result<()> {
    write_agent_env_file(dsh_home, proxy_base, api_key)?;

    let mut yaml = String::from("# Generated by ai-fence-cli; do not edit.\n");

    // Pin the managed default model so bare headless/web launches cannot fall
    // back to a built-in provider route picked up through ambient provider
    // credentials.
    if let Some(default_model) = default_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        if let Some(provider) = providers
            .iter()
            .find(|provider| provider.models.iter().any(|model| model == default_model))
        {
            yaml.push_str("agent-default-model:\n");
            yaml.push_str(&format!("  provider: {}\n", yaml_scalar(&provider.name)));
            yaml.push_str(&format!("  model: {}\n", yaml_scalar(default_model)));
        }
    }

    yaml.push_str("llm-pi-ai:\n");
    yaml.push_str("  providers:\n");
    for provider in providers {
        if provider.name.trim().is_empty() {
            anyhow::bail!("dsh provider name must not be empty");
        }
        yaml.push_str(&format!("    {}:\n", yaml_scalar(&provider.name)));
        yaml.push_str(&format!(
            "      apiKeyEnv: {}\n",
            yaml_scalar(DSH_API_KEY_ENV_VAR)
        ));
        yaml.push_str(&format!(
            "      api: {}\n",
            yaml_scalar(provider.api.as_str())
        ));
        yaml.push_str(&format!(
            "      baseURL: {}\n",
            yaml_scalar(&provider.base_url)
        ));
        yaml.push_str("      models:\n");
        for model in &provider.models {
            yaml.push_str(&format!("        - id: {}\n", yaml_scalar(model)));
        }
    }

    fs::write(dsh_home.join("settings.yaml"), yaml).with_context(|| {
        format!(
            "failed to write {}",
            dsh_home.join("settings.yaml").display()
        )
    })
}

pub const KIMI_SHARE_DIR_ENV_VAR: &str = "KIMI_SHARE_DIR";

/// The user-level harness home the Kimi CLI resolves when it is not managed
/// by AI Fence. Kimi relocates its whole state dir (config.toml, sessions,
/// logs) through `KIMI_SHARE_DIR`.
pub fn default_user_kimi_home() -> Option<PathBuf> {
    std::env::var_os(KIMI_SHARE_DIR_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".kimi")))
}

/// Resolve a run-scoped Kimi share dir and refuse the normal user home.
/// The caller must create the returned directory before launching.
pub fn resolve_kimi_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let kimi_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".kimi"));
    if let Some(default_home) = default_user_kimi_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&kimi_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default KIMI_SHARE_DIR {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(kimi_home)
}

/// Transport API declared for a Kimi CLI provider entry. Values match the
/// `type` literals of kimi's provider schema (`kimi_cli.llm.ProviderType`);
/// kimi has no generic `openai` alias, only the legacy chat-completions and
/// responses variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiProviderType {
    OpenAiLegacy,
    OpenAiResponses,
    Anthropic,
}

impl KimiProviderType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiLegacy => "openai_legacy",
            Self::OpenAiResponses => "openai_responses",
            Self::Anthropic => "anthropic",
        }
    }
}

/// One model exposed through a managed Kimi CLI provider. The wire model id
/// is the fence route id; kimi sends it verbatim to the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiModelConfig {
    pub id: String,
    pub max_context_size: u64,
}

/// One custom Kimi CLI provider entry for the managed config.toml, grouped by
/// transport like the pi/dsh providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiProviderConfig {
    pub name: String,
    pub base_url: String,
    pub provider_type: KimiProviderType,
    pub models: Vec<KimiModelConfig>,
}

/// The config.toml `[models]` key for a wire model id inside the given
/// providers, i.e. `<provider>/<wire-id>`. Returns None when no declared
/// provider hosts the id. Both the settings writer (default-model pin) and
/// the launcher's argv injection share this mapping so `-m` always resolves
/// to an entry that exists in the generated file.
pub fn kimi_model_key(providers: &[KimiProviderConfig], model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    providers
        .iter()
        .find(|provider| provider.models.iter().any(|entry| entry.id == model))
        .map(|provider| format!("{}/{model}", provider.name))
}

/// Write the managed Kimi CLI configuration pointing the agent at the AI
/// Fence proxy. Kimi resolves providers only through its config.toml (the
/// anthropic lane ignores base-url environment variables entirely), so every
/// fence route is declared as a custom provider grouped by transport; the key
/// is embedded inline because kimi has no `${ENV}` expansion in api_key. The
/// env file is still written for parity with the other managed agents.
pub fn write_kimi_settings(
    kimi_home: &Path,
    proxy_base: &str,
    api_key: Option<&str>,
    providers: &[KimiProviderConfig],
    default_model: Option<&str>,
) -> Result<()> {
    write_agent_env_file(kimi_home, proxy_base, api_key)?;

    let mut root = toml::Table::new();
    // Pin the managed default model so bare `kimi` launches cannot fall back
    // to a built-in provider route picked up from ambient credentials.
    if let Some(key) = default_model.and_then(|model| kimi_model_key(providers, model)) {
        root.insert(
            "default_model".to_string(),
            toml::Value::String(key.clone()),
        );
    }

    let mut models = toml::Table::new();
    let mut providers_table = toml::Table::new();
    for provider in providers {
        if provider.name.trim().is_empty() {
            anyhow::bail!("kimi provider name must not be empty");
        }
        let mut entry = toml::Table::new();
        entry.insert(
            "type".to_string(),
            toml::Value::String(provider.provider_type.as_str().to_string()),
        );
        entry.insert(
            "base_url".to_string(),
            toml::Value::String(provider.base_url.clone()),
        );
        entry.insert(
            "api_key".to_string(),
            toml::Value::String(api_key.unwrap_or_default().to_string()),
        );
        providers_table.insert(provider.name.clone(), toml::Value::Table(entry));

        for model in &provider.models {
            if model.id.trim().is_empty() {
                anyhow::bail!("kimi model id must not be empty");
            }
            let mut model_entry = toml::Table::new();
            model_entry.insert(
                "provider".to_string(),
                toml::Value::String(provider.name.clone()),
            );
            model_entry.insert("model".to_string(), toml::Value::String(model.id.clone()));
            model_entry.insert(
                "max_context_size".to_string(),
                toml::Value::Integer(model.max_context_size as i64),
            );
            models.insert(format!("{}/{}", provider.name, model.id), {
                toml::Value::Table(model_entry)
            });
        }
    }
    root.insert("models".to_string(), toml::Value::Table(models));
    root.insert("providers".to_string(), toml::Value::Table(providers_table));

    let serialized = toml::to_string_pretty(&toml::Value::Table(root))?;
    fs::write(kimi_home.join("config.toml"), serialized).with_context(|| {
        format!(
            "failed to write {}",
            kimi_home.join("config.toml").display()
        )
    })
}

/// The env var that relocates the GitHub Copilot CLI's config/state home.
pub const COPILOT_HOME_ENV_VAR: &str = "COPILOT_HOME";

/// The user-level harness home the GitHub Copilot CLI resolves when it is not
/// managed by AI Fence. Copilot relocates its whole config/state home through
/// `COPILOT_HOME` (default `~/.copilot`).
pub fn default_user_copilot_home() -> Option<PathBuf> {
    std::env::var_os(COPILOT_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".copilot")))
}

/// Resolve a run-scoped Copilot home and refuse the normal user home. The
/// caller must create the returned directory before launching.
pub fn resolve_copilot_home(
    config_dir: &Path,
    explicit: Option<&Path>,
    managed_dir_label: &str,
) -> Result<PathBuf> {
    let copilot_home = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(".copilot"));
    if let Some(default_home) = default_user_copilot_home() {
        let default_home = normalize_path_lexical(&default_home)?;
        let requested = normalize_path_lexical(&copilot_home)?;
        if requested == default_home {
            anyhow::bail!(
                "refusing to use default COPILOT_HOME {}; use the managed {managed_dir_label} directory",
                default_home.display()
            );
        }
    }
    Ok(copilot_home)
}

/// Custom-provider dialect of the GitHub Copilot CLI (`COPILOT_PROVIDER_TYPE`
/// literal). Azure is not exposed because AI Fence has no azure-shaped lane;
/// keys are sent as `Authorization` (openai) or `x-api-key` (anthropic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotProviderType {
    OpenAi,
    Anthropic,
}

impl CopilotProviderType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// One custom Copilot provider configuration, applied through environment
/// variables for a single run. Copilot configures exactly one custom provider
/// per process, so the launcher picks the lane matching the effective model's
/// wire transport instead of declaring all three like pi/kimi/junie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotProviderConfig {
    pub provider_type: CopilotProviderType,
    /// Endpoint base URL. The openai lane appends `/chat/completions` (or the
    /// responses path), so pass `<proxy>/v1`; the anthropic lane appends
    /// `/v1/messages`, so pass the proxy root.
    pub base_url: String,
    /// Set to select the Responses wire API (`COPILOT_PROVIDER_WIRE_API`);
    /// only meaningful for the openai provider type.
    pub responses_wire_api: bool,
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
    fn interpreter_launch_defaults_and_detects_binary_aliases() {
        let launch = resolve_launch(AgentKind::Interpreter, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Interpreter);
        assert_eq!(launch.command, vec!["interpreter".to_string()]);

        let command = vec!["/usr/local/bin/i".to_string(), "review".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Interpreter);
        assert_eq!(detected.command, command);

        let with_implicit_binary =
            resolve_launch(AgentKind::Interpreter, &["exec".to_string()]).expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["interpreter".to_string(), "exec".to_string()]
        );
    }

    #[test]
    fn generic_agent_requires_command() {
        let error = resolve_launch(AgentKind::Generic, &[]).expect_err("error");
        assert!(error.to_string().contains("requires --agent codex/claude"));
    }

    #[test]
    fn junie_launch_defaults_and_detects_binary() {
        let launch = resolve_launch(AgentKind::Junie, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Junie);
        assert_eq!(launch.command, vec!["junie".to_string()]);

        let command = vec!["/usr/local/bin/junie".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Junie);

        let with_implicit_binary =
            resolve_launch(AgentKind::Junie, &["exec".to_string()]).expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["junie".to_string(), "exec".to_string()]
        );
    }

    #[test]
    fn resolve_junie_home_defaults_to_run_scoped_dot_junie() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let junie_home = resolve_junie_home(&run_dir, None, ".ai-fence/runs").expect("junie home");

        assert_eq!(junie_home, run_dir.join(".junie"));
    }

    #[test]
    fn pi_launch_defaults_and_detects_binary() {
        let launch = resolve_launch(AgentKind::Pi, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Pi);
        assert_eq!(launch.command, vec!["pi".to_string()]);

        let command = vec!["/usr/local/bin/pi".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Pi);

        let with_implicit_binary =
            resolve_launch(AgentKind::Pi, &["--version".to_string()]).expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["pi".to_string(), "--version".to_string()]
        );
    }

    #[test]
    fn resolve_pi_home_defaults_to_run_scoped_dot_pi_agent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let pi_home = resolve_pi_home(&run_dir, None, ".ai-fence/runs").expect("pi home");

        assert_eq!(pi_home, run_dir.join(".pi").join("agent"));
    }

    #[test]
    fn dsh_launch_defaults_and_detects_binary() {
        let launch = resolve_launch(AgentKind::Dsh, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Dsh);
        assert_eq!(launch.command, vec!["dsh".to_string()]);

        let command = vec!["/usr/local/bin/dsh".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Dsh);

        let with_implicit_binary =
            resolve_launch(AgentKind::Dsh, &["web".to_string()]).expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["dsh".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn resolve_dsh_home_defaults_to_run_scoped_dot_dsh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let dsh_home = resolve_dsh_home(&run_dir, None, ".ai-fence/runs").expect("dsh home");

        assert_eq!(dsh_home, run_dir.join(".dsh"));
    }

    #[test]
    fn write_dsh_settings_writes_grouped_providers_and_default_pin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dsh_home = temp.path().join("dsh-home");
        fs::create_dir_all(&dsh_home).expect("create dsh home");

        let providers = vec![
            PiProviderConfig {
                name: "fence-chat".to_string(),
                base_url: "http://127.0.0.1:1234/v1".to_string(),
                api: PiProviderApi::OpenAiCompletions,
                models: vec!["kimi/completions/k3".to_string()],
            },
            PiProviderConfig {
                name: "fence-messages".to_string(),
                base_url: "http://127.0.0.1:1234".to_string(),
                api: PiProviderApi::AnthropicMessages,
                models: vec!["zai-anthropic/glm-5.3".to_string()],
            },
        ];
        write_dsh_settings(
            &dsh_home,
            "http://127.0.0.1:1234",
            Some("test-key"),
            &providers,
            Some("zai-anthropic/glm-5.3"),
        )
        .expect("write settings");

        let env = fs::read_to_string(dsh_home.join("env")).expect("read env");
        assert!(env.contains("OPENAI_BASE_URL=http://127.0.0.1:1234/v1"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:1234"));

        let settings =
            fs::read_to_string(dsh_home.join("settings.yaml")).expect("read settings.yaml");

        // The default selection pins the fence route that declares the model.
        assert!(settings.contains("agent-default-model:\n"));
        assert!(settings.contains("  provider: 'fence-messages'\n"));
        assert!(settings.contains("  model: 'zai-anthropic/glm-5.3'\n"));

        // Provider routes are grouped by transport and reference the key
        // through apiKeyEnv; no literal secret lands in the file.
        assert!(settings.contains("    'fence-chat':\n"));
        assert!(settings.contains("      apiKeyEnv: 'AI_FENCE_DSH_API_KEY'\n"));
        assert!(settings.contains("      api: 'openai-completions'\n"));
        assert!(settings.contains("      baseURL: 'http://127.0.0.1:1234/v1'\n"));
        assert!(settings.contains("        - id: 'kimi/completions/k3'\n"));
        assert!(settings.contains("    'fence-messages':\n"));
        assert!(settings.contains("      api: 'anthropic-messages'\n"));
        assert!(settings.contains("      baseURL: 'http://127.0.0.1:1234'\n"));
        assert!(settings.contains("        - id: 'zai-anthropic/glm-5.3'\n"));
        assert!(!settings.contains("test-key"));

        // Without a resolvable default model the pin section is omitted but
        // the provider routes remain declared.
        let keyless = temp.path().join("dsh-home-keyless");
        fs::create_dir_all(&keyless).expect("create keyless home");
        write_dsh_settings(&keyless, "http://127.0.0.1:1234", None, &providers, None)
            .expect("write keyless settings");
        let keyless_settings =
            fs::read_to_string(keyless.join("settings.yaml")).expect("read keyless settings.yaml");
        assert!(!keyless_settings.contains("agent-default-model:"));
        assert!(keyless_settings.contains("llm-pi-ai:\n"));
    }

    #[test]
    fn kimi_launch_defaults_and_detects_binary() {
        let launch = resolve_launch(AgentKind::Kimi, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Kimi);
        assert_eq!(launch.command, vec!["kimi".to_string()]);

        let command = vec!["/usr/local/bin/kimi".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Kimi);

        let with_implicit_binary =
            resolve_launch(AgentKind::Kimi, &["--print".to_string()]).expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["kimi".to_string(), "--print".to_string()]
        );
    }

    #[test]
    fn resolve_kimi_home_defaults_to_run_scoped_dot_kimi() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let kimi_home = resolve_kimi_home(&run_dir, None, ".ai-fence/runs").expect("kimi home");

        assert_eq!(kimi_home, run_dir.join(".kimi"));
    }

    #[test]
    fn copilot_launch_defaults_and_detects_binary() {
        let launch = resolve_launch(AgentKind::Copilot, &[]).expect("launch");
        assert_eq!(launch.agent, ResolvedAgent::Copilot);
        assert_eq!(launch.command, vec!["copilot".to_string()]);

        let command = vec!["/opt/copilot/bin/copilot".to_string()];
        let detected = resolve_launch(AgentKind::Auto, &command).expect("launch");
        assert_eq!(detected.agent, ResolvedAgent::Copilot);

        let with_implicit_binary =
            resolve_launch(AgentKind::Copilot, &["-p".to_string(), "hi".to_string()])
                .expect("launch");
        assert_eq!(
            with_implicit_binary.command,
            vec!["copilot".to_string(), "-p".to_string(), "hi".to_string()]
        );
    }

    #[test]
    fn resolve_copilot_home_defaults_to_run_scoped_dot_copilot_and_refuses_user_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run_dir = temp.path().join("runs").join("run-1");

        let copilot_home =
            resolve_copilot_home(&run_dir, None, ".ai-fence/runs").expect("copilot home");
        assert_eq!(copilot_home, run_dir.join(".copilot"));

        // The default user home must never be adopted as a managed dir.
        let user_home = temp.path().join("home");
        std::env::set_var(COPILOT_HOME_ENV_VAR, &user_home);
        let result = resolve_copilot_home(&run_dir, Some(&user_home), ".ai-fence/runs");
        std::env::remove_var(COPILOT_HOME_ENV_VAR);
        assert!(result.is_err());
    }

    #[test]
    fn kimi_model_key_maps_wire_id_to_provider_entry() {
        let providers = vec![KimiProviderConfig {
            name: "fence-chat".to_string(),
            base_url: "http://127.0.0.1:1234/v1".to_string(),
            provider_type: KimiProviderType::OpenAiLegacy,
            models: vec![KimiModelConfig {
                id: "kimi/completions/k3-256k".to_string(),
                max_context_size: 131_072,
            }],
        }];

        assert_eq!(
            kimi_model_key(&providers, "kimi/completions/k3-256k"),
            Some("fence-chat/kimi/completions/k3-256k".to_string())
        );
        assert_eq!(kimi_model_key(&providers, "zai-anthropic/glm-5.3"), None);
        assert_eq!(kimi_model_key(&providers, "   "), None);
    }

    #[test]
    fn write_kimi_settings_writes_grouped_providers_and_default_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kimi_home = temp.path().join("kimi-home");
        fs::create_dir_all(&kimi_home).expect("create kimi home");

        let providers = vec![
            KimiProviderConfig {
                name: "fence-chat".to_string(),
                base_url: "http://127.0.0.1:1234/v1".to_string(),
                provider_type: KimiProviderType::OpenAiLegacy,
                models: vec![
                    KimiModelConfig {
                        id: "kimi/completions/k3-256k".to_string(),
                        max_context_size: 131_072,
                    },
                    KimiModelConfig {
                        id: "openai/gpt-5.2".to_string(),
                        max_context_size: 131_072,
                    },
                ],
            },
            KimiProviderConfig {
                name: "fence-messages".to_string(),
                base_url: "http://127.0.0.1:1234".to_string(),
                provider_type: KimiProviderType::Anthropic,
                models: vec![KimiModelConfig {
                    id: "zai-anthropic/glm-5.3".to_string(),
                    max_context_size: 131_072,
                }],
            },
        ];
        write_kimi_settings(
            &kimi_home,
            "http://127.0.0.1:1234",
            Some("test-key"),
            &providers,
            Some("zai-anthropic/glm-5.3"),
        )
        .expect("write settings");

        let env = fs::read_to_string(kimi_home.join("env")).expect("read env");
        assert!(env.contains("OPENAI_BASE_URL=http://127.0.0.1:1234/v1"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:1234"));

        // The generated file must stay parseable TOML; the default selection
        // pins the fence route that declares the model.
        let config = fs::read_to_string(kimi_home.join("config.toml")).expect("read config.toml");
        let parsed: toml::Value = toml::from_str(&config).expect("parse generated config.toml");
        assert_eq!(
            parsed["default_model"].as_str(),
            Some("fence-messages/zai-anthropic/glm-5.3")
        );

        assert_eq!(
            parsed["providers"]["fence-chat"]["type"].as_str(),
            Some("openai_legacy")
        );
        assert_eq!(
            parsed["providers"]["fence-chat"]["base_url"].as_str(),
            Some("http://127.0.0.1:1234/v1")
        );
        assert_eq!(
            parsed["providers"]["fence-messages"]["type"].as_str(),
            Some("anthropic")
        );
        assert_eq!(
            parsed["providers"]["fence-messages"]["base_url"].as_str(),
            Some("http://127.0.0.1:1234")
        );
        assert_eq!(
            parsed["models"]["fence-chat/kimi/completions/k3-256k"]["provider"].as_str(),
            Some("fence-chat")
        );
        assert_eq!(
            parsed["models"]["fence-chat/kimi/completions/k3-256k"]["model"].as_str(),
            Some("kimi/completions/k3-256k")
        );
        assert_eq!(
            parsed["models"]["fence-chat/kimi/completions/k3-256k"]["max_context_size"]
                .as_integer(),
            Some(131_072)
        );
        assert_eq!(
            parsed["models"]["fence-messages/zai-anthropic/glm-5.3"]["model"].as_str(),
            Some("zai-anthropic/glm-5.3")
        );
    }

    #[test]
    fn write_kimi_settings_omits_default_pin_without_resolvable_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kimi_home = temp.path().join("kimi-home-keyless");
        fs::create_dir_all(&kimi_home).expect("create keyless home");

        write_kimi_settings(&kimi_home, "http://127.0.0.1:1234", None, &[], None)
            .expect("write keyless settings");
        let config =
            fs::read_to_string(kimi_home.join("config.toml")).expect("read keyless config.toml");
        assert!(!config.contains("default_model"));
        assert!(config.contains("[providers]"));
    }

    #[test]
    fn write_pi_settings_writes_grouped_providers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pi_home = temp.path().join("pi-agent");
        fs::create_dir_all(&pi_home).expect("create pi home");

        let providers = vec![
            PiProviderConfig {
                name: "fence-chat".to_string(),
                base_url: "http://127.0.0.1:1234/v1".to_string(),
                api: PiProviderApi::OpenAiCompletions,
                models: vec!["kimi/completions/k3".to_string()],
            },
            PiProviderConfig {
                name: "fence-messages".to_string(),
                base_url: "http://127.0.0.1:1234".to_string(),
                api: PiProviderApi::AnthropicMessages,
                models: vec!["zai-anthropic/glm-5.3".to_string()],
            },
        ];
        write_pi_settings(
            &pi_home,
            "http://127.0.0.1:1234",
            Some("test-key"),
            &providers,
            Some("zai-anthropic/glm-5.3"),
        )
        .expect("write settings");

        let env = fs::read_to_string(pi_home.join("env")).expect("read env");
        assert!(env.contains("OPENAI_BASE_URL=http://127.0.0.1:1234/v1"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:1234"));

        let models: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(pi_home.join("models.json")).expect("read models.json"),
        )
        .expect("parse models.json");
        let chat = &models["providers"]["fence-chat"];
        assert_eq!(chat["baseUrl"], "http://127.0.0.1:1234/v1");
        assert_eq!(chat["api"], "openai-completions");
        assert_eq!(chat["apiKey"], "test-key");
        assert_eq!(chat["models"][0]["id"], "kimi/completions/k3");
        let messages = &models["providers"]["fence-messages"];
        assert_eq!(messages["baseUrl"], "http://127.0.0.1:1234");
        assert_eq!(messages["api"], "anthropic-messages");
        assert_eq!(messages["models"][0]["id"], "zai-anthropic/glm-5.3");

        // The default selection pins the provider that declares the model.
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(pi_home.join("settings.json")).expect("read settings.json"),
        )
        .expect("parse settings.json");
        assert_eq!(settings["defaultProvider"], "fence-messages");
        assert_eq!(settings["defaultModel"], "zai-anthropic/glm-5.3");

        // Without an api key the providers are still declared but omit the
        // key field, matching the env-file behavior of the other agents.
        let keyless = temp.path().join("pi-agent-keyless");
        fs::create_dir_all(&keyless).expect("create keyless home");
        write_pi_settings(&keyless, "http://127.0.0.1:1234", None, &providers, None)
            .expect("write keyless settings");
        let keyless_models: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(keyless.join("models.json")).expect("read keyless models.json"),
        )
        .expect("parse keyless models.json");
        assert!(keyless_models["providers"]["fence-chat"]
            .get("apiKey")
            .is_none());
    }

    #[test]
    fn write_junie_settings_points_at_proxy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let junie_home = temp.path().join("junie");
        // The launcher creates the managed home before writing settings.
        fs::create_dir_all(&junie_home).expect("create junie home");

        write_junie_settings(&junie_home, "http://127.0.0.1:1234", Some("test-key"))
            .expect("write settings");

        let env = fs::read_to_string(junie_home.join("env")).expect("read env");
        assert!(env.contains("OPENAI_BASE_URL=http://127.0.0.1:1234/v1"));
        assert!(env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:1234"));
        assert!(env.contains("OPENAI_API_KEY=test-key"));
    }

    #[test]
    fn junie_model_stem_sanitizes_route_ids() {
        assert_eq!(
            junie_model_stem("zai-anthropic/glm-5.3"),
            "zai-anthropic-glm-5.3"
        );
        assert_eq!(
            junie_model_stem("kimi/completions/k3"),
            "kimi-completions-k3"
        );
        // Leading/trailing separators and empty ids stay valid file stems.
        assert_eq!(junie_model_stem("/weird id/"), "weird-id");
        assert_eq!(junie_model_stem("///"), "fenced-model");
    }

    #[test]
    fn write_junie_models_writes_byok_files_with_env_key_refs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let junie_home = temp.path().join("junie");
        fs::create_dir_all(&junie_home).expect("create junie home");

        write_junie_models(
            &junie_home,
            &[
                JunieModelConfig {
                    id: "zai-anthropic/glm-5.3".to_string(),
                    display_name: "GLM 5.3".to_string(),
                    base_url: "http://127.0.0.1:1234/v1/messages".to_string(),
                    api_type: JunieApiType::Anthropic,
                },
                JunieModelConfig {
                    id: "kimi/completions/k3".to_string(),
                    display_name: "Kimi K3".to_string(),
                    base_url: "http://127.0.0.1:1234/v1/chat/completions".to_string(),
                    api_type: JunieApiType::OpenAiCompletion,
                },
            ],
        )
        .expect("write models");

        let messages: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(junie_home.join("models/zai-anthropic-glm-5.3.json"))
                .expect("read glm model file"),
        )
        .expect("parse glm model file");
        assert_eq!(messages["id"], "zai-anthropic/glm-5.3");
        assert_eq!(messages["baseUrl"], "http://127.0.0.1:1234/v1/messages");
        assert_eq!(messages["apiKey"], "${OPENAI_API_KEY}");
        assert_eq!(messages["apiType"], "Anthropic");

        let completions: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(junie_home.join("models/kimi-completions-k3.json"))
                .expect("read kimi model file"),
        )
        .expect("parse kimi model file");
        assert_eq!(
            completions["baseUrl"],
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        assert_eq!(completions["apiType"], "OpenAICompletion");
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
        assert_eq!(profile.sqlite_dir, profile.root_dir.join("sqlite"));
        assert_eq!(profile.lock_path, profile.root_dir.join(".sync.lock"));
        assert_eq!(
            profile.metadata_path,
            profile.root_dir.join("metadata.json")
        );

        let interpreter = resolve_agent_profile(temp.path(), "default", ResolvedAgent::Interpreter)
            .expect("interpreter profile");
        assert_eq!(
            interpreter.root_dir,
            temp.path()
                .join("profiles")
                .join("default")
                .join("interpreter")
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn current_process_is_detected_for_profile_locks() {
        assert!(process_exists(std::process::id()));
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

        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(restored.join("sessions"))
                .expect("restored sessions metadata")
                .file_type()
                .is_symlink(),
            "managed sessions should use live durable storage"
        );
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
        #[cfg(unix)]
        {
            let crash_durable_session = restored
                .join("sessions")
                .join("2026")
                .join("06")
                .join("09")
                .join("rollout-survives-crash.jsonl");
            fs::create_dir_all(crash_durable_session.parent().expect("session parent"))
                .expect("create live session directory");
            fs::write(&crash_durable_session, "live\n").expect("write live session");
            assert_eq!(
                fs::read_to_string(
                    profile
                        .state_dir
                        .join("sessions")
                        .join("2026")
                        .join("06")
                        .join("09")
                        .join("rollout-survives-crash.jsonl")
                )
                .expect("durable live session"),
                "live\n"
            );
            fs::remove_dir_all(restored.parent().expect("restored parent"))
                .expect("remove simulated crashed runtime");
            assert!(
                profile
                    .state_dir
                    .join("sessions")
                    .join("2026")
                    .join("06")
                    .join("09")
                    .join("rollout-survives-crash.jsonl")
                    .is_file(),
                "removing a crashed runtime must not remove durable session data"
            );
        }
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
    fn codex_profile_sqlite_home_is_durable_and_profile_scoped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let default =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");
        let other =
            resolve_agent_profile(temp.path(), "other", ResolvedAgent::Codex).expect("profile");

        let sqlite_home = prepare_codex_profile_sqlite_home(&default).expect("sqlite home");
        assert_eq!(sqlite_home, default.sqlite_dir);
        assert_ne!(sqlite_home, other.sqlite_dir);
        fs::write(sqlite_home.join("state_5.sqlite"), "durable index")
            .expect("durable state index");

        let runtime = temp.path().join("run").join(".codex");
        sync_profile_state_to_runtime(&default, &runtime).expect("sync in");
        fs::remove_dir_all(runtime.parent().expect("runtime parent"))
            .expect("simulate crashed runtime cleanup");

        assert_eq!(
            fs::read_to_string(default.sqlite_dir.join("state_5.sqlite"))
                .expect("persistent state index"),
            "durable index"
        );
    }

    #[test]
    fn codex_profile_home_is_stable_and_profile_scoped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let default =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");
        let other =
            resolve_agent_profile(temp.path(), "other", ResolvedAgent::Codex).expect("profile");

        let codex_home = prepare_codex_profile_home(&default).expect("Codex home");

        assert_eq!(codex_home, default.state_dir);
        assert_ne!(codex_home, other.state_dir);
        assert!(codex_home.is_dir());
        assert!(!default.sqlite_dir.exists());
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &fs::metadata(codex_home)
                    .expect("Codex home metadata")
                    .permissions()
            ) & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_profile_writes_new_sessions_directly_to_durable_storage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = resolve_agent_profile(temp.path(), "kimi", ResolvedAgent::Interpreter)
            .expect("profile");
        let runtime = temp.path().join("run").join(".openinterpreter");

        sync_profile_state_to_runtime(&profile, &runtime).expect("sync in");
        let runtime_sessions = runtime.join("sessions");
        assert!(fs::symlink_metadata(&runtime_sessions)
            .expect("runtime sessions")
            .file_type()
            .is_symlink());

        let transcript = runtime_sessions.join("rollout-kimi.jsonl");
        fs::write(&transcript, "tool result\n").expect("write transcript");
        assert_eq!(
            fs::read_to_string(profile.state_dir.join("sessions/rollout-kimi.jsonl"))
                .expect("durable transcript"),
            "tool result\n"
        );

        fs::remove_dir_all(runtime.parent().expect("runtime parent"))
            .expect("remove simulated crashed runtime");
        assert!(profile
            .state_dir
            .join("sessions/rollout-kimi.jsonl")
            .is_file());
    }

    #[test]
    fn codex_profile_sync_persists_safe_model_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile =
            resolve_agent_profile(temp.path(), "default", ResolvedAgent::Codex).expect("profile");
        fs::create_dir_all(&profile.root_dir).expect("profile dir");
        fs::write(
            codex_profile_config_path(&profile),
            "custom_profile_setting = \"keep\"\n",
        )
        .expect("profile config");

        let runtime = temp.path().join("run").join(".codex");
        fs::create_dir_all(&runtime).expect("runtime");
        fs::write(
            runtime.join("config.toml"),
            r#"
model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"
secret_config = "do-not-copy"

[model_providers.ai_fence]
base_url = "http://127.0.0.1:9999/v1"
env_key = "SECRET_TOKEN"
"#,
        )
        .expect("runtime config");

        sync_runtime_state_to_profile(&profile, &runtime).expect("sync out");

        let profile_config =
            fs::read_to_string(codex_profile_config_path(&profile)).expect("profile config");
        let profile_config = profile_config
            .parse::<toml::Value>()
            .expect("parse profile config");
        assert_eq!(profile_config["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(
            profile_config["model_reasoning_effort"].as_str(),
            Some("xhigh")
        );
        assert_eq!(
            profile_config["custom_profile_setting"].as_str(),
            Some("keep")
        );
        assert!(profile_config.get("secret_config").is_none());
        assert!(profile_config.get("model_providers").is_none());

        let restored = temp.path().join("restored").join(".codex");
        let selected_model = write_codex_config_with_profile_and_default_model(
            &restored,
            "http://127.0.0.1:1234",
            CodexModelSelection {
                explicit_model: None,
                default_model: Some("gpt-5.5"),
            },
            None,
            false,
            CodexProviderAuth::EnvKey,
            Some(&profile),
        )
        .expect("write restored config");
        assert_eq!(selected_model.as_deref(), Some("gpt-5.6-sol"));

        let restored_config =
            fs::read_to_string(restored.join("config.toml")).expect("restored config");
        let restored_config = restored_config
            .parse::<toml::Value>()
            .expect("parse restored config");
        assert_eq!(restored_config["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(
            restored_config["model_reasoning_effort"].as_str(),
            Some("xhigh")
        );
        assert_eq!(
            restored_config["custom_profile_setting"].as_str(),
            Some("keep")
        );
        assert!(restored_config.get("secret_config").is_none());
        assert_eq!(
            restored_config["model_providers"]["ai_fence"]["base_url"].as_str(),
            Some("http://127.0.0.1:1234/v1")
        );

        let explicitly_overridden = temp.path().join("explicit").join(".codex");
        let selected_model = write_codex_config_with_profile_and_default_model(
            &explicitly_overridden,
            "http://127.0.0.1:1234",
            CodexModelSelection {
                explicit_model: Some("gpt-5.7"),
                default_model: Some("gpt-5.5"),
            },
            None,
            false,
            CodexProviderAuth::EnvKey,
            Some(&profile),
        )
        .expect("write explicitly overridden config");
        assert_eq!(selected_model.as_deref(), Some("gpt-5.7"));
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
sqlite_home = "/tmp/user-managed-sqlite"

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
        assert!(config.contains("sqlite_home = \"/tmp/user-managed-sqlite\""));
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
sqlite_home = "/tmp/stale-profile-sqlite"

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
                catalog_models: &[],
            },
        )
        .expect("write config");

        let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
        let parsed = config.parse::<toml::Value>().expect("parse config");
        assert_eq!(parsed["custom_user_setting"].as_str(), Some("profile"));
        assert_eq!(parsed["custom_profile_setting"].as_str(), Some("keep"));
        assert!(parsed.get("sqlite_home").is_none());
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
        assert_eq!(model["display_name"], "K2.7 Coding");
        assert_eq!(model["supported_in_api"], true);
        assert_eq!(model["truncation_policy"]["mode"], "tokens");
        assert_eq!(model["context_window"], 262144);
    }

    #[test]
    fn write_codex_config_generates_ordered_multi_model_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_home = temp.path().join("codex");
        let models = vec![
            "kimi/completions/k3".to_string(),
            "zai-anthropic/glm-5".to_string(),
            "openai/gpt-5.6-sol".to_string(),
        ];

        write_codex_config_with_model_selection(
            &codex_home,
            "http://127.0.0.1:8181",
            CodexModelSelection {
                explicit_model: None,
                default_model: Some("kimi/completions/k3"),
            },
            None,
            false,
            CodexProviderAuth::EnvKey,
            CodexConfigExtras {
                profile: None,
                mcp_servers: &[],
                catalog_models: &models,
            },
        )
        .expect("write config");

        let catalog: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(codex_home.join("model-catalog.json")).expect("catalog"),
        )
        .expect("parse catalog");
        let slugs = catalog["models"]
            .as_array()
            .expect("models")
            .iter()
            .map(|model| model["slug"].as_str().expect("slug"))
            .collect::<Vec<_>>();
        assert_eq!(
            slugs,
            vec![
                "kimi/completions/k3",
                "zai-anthropic/glm-5",
                "openai/gpt-5.6-sol"
            ]
        );
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
        assert!(
            settings.contains("\"CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT\": \"1\"")
        );
    }

    #[test]
    fn write_claude_settings_preserves_template_window_enforcement_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".claude");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join("settings.json"),
            r#"{"env":{"CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT":"0"}}"#,
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
        assert!(
            settings.contains("\"CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT\": \"0\"")
        );
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
    fn claude_profile_mcp_overlay_merges_into_runtime_user_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join(".ai-fence");
        let template_dir = config_dir.join(".claude");
        fs::create_dir_all(&template_dir).expect("template dir");
        fs::write(
            template_dir.join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"global_docs":{"type":"http","url":"https://global.example/mcp"},"shared":{"command":"global.sh"}}}"#,
        )
        .expect("write template");

        let profile =
            resolve_agent_profile(&config_dir, "kimi", ResolvedAgent::Claude).expect("profile");
        fs::create_dir_all(&profile.root_dir).expect("profile dir");
        fs::write(
            claude_profile_user_config_path(&profile),
            r#"{"mcpServers":{"profile_search":{"type":"http","url":"https://profile.example/mcp"},"shared":{"command":"profile.sh"}}}"#,
        )
        .expect("write profile overlay");
        upsert_claude_profile_mcp_server(
            &profile,
            &AgentMcpServer::streamable_http(
                "proj_creator_dev",
                "https://create-dev.matthid.de/api/mcp",
            )
            .with_bearer_token_env_var("PROJ_CREATOR_DEV_API_KEY"),
        )
        .expect("profile mcp");

        let overlay: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(claude_profile_user_config_path(&profile)).expect("overlay"),
        )
        .expect("parse overlay");
        assert!(overlay["mcpServers"]["profile_search"]["url"]
            .as_str()
            .is_some_and(|url| url == "https://profile.example/mcp"));

        let claude_dir = temp.path().join("run").join(".claude");
        write_claude_user_config_for_profile(&claude_dir, Some(&config_dir), Some(&profile))
            .expect("write user config");

        let user_config: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(claude_dir.join(".claude.json")).expect("user config"),
        )
        .expect("parse user config");
        assert_eq!(user_config["theme"], "dark");
        assert_eq!(
            user_config["mcpServers"]["global_docs"]["url"],
            "https://global.example/mcp"
        );
        assert_eq!(
            user_config["mcpServers"]["profile_search"]["url"],
            "https://profile.example/mcp"
        );
        assert_eq!(user_config["mcpServers"]["shared"]["command"], "profile.sh");
        assert_eq!(
            user_config["mcpServers"]["proj_creator_dev"]["url"],
            "https://create-dev.matthid.de/api/mcp"
        );
        assert_eq!(
            user_config["mcpServers"]["proj_creator_dev"]["headers"]["Authorization"],
            "Bearer ${PROJ_CREATOR_DEV_API_KEY}"
        );
    }

    #[test]
    fn upsert_claude_profile_mcp_server_rejects_non_claude_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = resolve_agent_profile(temp.path(), "kimi", ResolvedAgent::Interpreter)
            .expect("profile");
        let error = upsert_claude_profile_mcp_server(
            &profile,
            &AgentMcpServer::stdio("local_tools", "node"),
        )
        .expect_err("interpreter profile must not accept a Claude overlay");
        assert!(error.to_string().contains("Claude agent profile"));
        assert!(!claude_profile_user_config_path(&profile).exists());
    }

    #[test]
    fn write_claude_settings_and_agents_expose_selected_models() {
        let temp = tempfile::tempdir().expect("tempdir");
        let claude_dir = temp.path().join(".claude");
        fs::create_dir_all(&claude_dir).expect("claude dir");
        let models = vec![
            "kimi/anthropic/k3".to_string(),
            "zai-anthropic/glm-5".to_string(),
            "openai/gpt-5.6-sol".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        ];

        write_claude_settings_with_models(
            &claude_dir,
            "http://127.0.0.1:8181",
            Some("kimi/anthropic/k3"),
            &models,
            None,
            false,
        )
        .expect("settings");
        write_claude_model_agents(&claude_dir, &models).expect("agents");

        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(claude_dir.join("settings.json")).expect("settings"),
        )
        .expect("parse settings");
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "kimi/anthropic/k3"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            "zai-anthropic/glm-5"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            "openai/gpt-5.6-sol"
        );
        let agents = fs::read_dir(claude_dir.join("agents"))
            .expect("agent directory")
            .collect::<std::io::Result<Vec<_>>>()
            .expect("agents");
        assert_eq!(agents.len(), models.len());
        assert!(agents.iter().any(|agent| {
            fs::read_to_string(agent.path())
                .is_ok_and(|contents| contents.contains("model: \"kimi/anthropic/k3\""))
        }));
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
model_catalog_json = "/tmp/stale-direct-model-catalog.json"
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
        assert!(!config.contains("model_catalog_json"));
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
