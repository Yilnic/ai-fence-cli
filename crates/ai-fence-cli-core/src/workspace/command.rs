use super::*;
use clap::Subcommand;
use std::process::Command;

#[derive(Debug, Subcommand)]
pub enum WorkspaceAction {
    /// Choose a workspace store once; place it on the project's Btrfs filesystem.
    Init {
        #[arg(long)]
        directory: PathBuf,
    },
    /// Create one persistent writable Btrfs clone, or reuse an existing name.
    #[command(alias = "new")]
    Create {
        name: String,
        /// Source subvolume; defaults to the original project, including from a workspace.
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Preview recovery into a new writable workspace; never restore over existing work.
    Recover {
        name: String,
        #[arg(long)]
        checkpoint: PathBuf,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
    /// Run in an existing workspace; arguments after -- are passed unchanged.
    #[command(alias = "exec")]
    Run {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(last = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Open your interactive shell in an existing workspace (including aliases).
    Shell {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Print only the existing workspace path, suitable for cd "$(...)".
    Path {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Return a Windows UNC path when running inside WSL.
        #[arg(long)]
        windows: bool,
    },
    /// List workspaces, active launches, changes and linked chats.
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        names: bool,
        #[arg(long)]
        include_removed: bool,
    },
    /// Review tracked changes since creation; list untracked files separately.
    Diff {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Preview removal. Active/pinned workspaces are always protected.
    Rm {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        apply: bool,
        /// Explicitly discard changed, ignored, untracked or unmerged workspace work.
        #[arg(long)]
        discard_changes: bool,
        /// Preserve chat history but mark its filesystem workspace removed.
        #[arg(long)]
        detach_chats: bool,
    },
    /// Keep a workspace protected from removal, or explicitly clear its pin.
    Pin {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        unpin: bool,
    },
    /// Print fw (Fence workspace) and fws (Fence workspace switch) shell helpers.
    ShellInit {
        #[arg(long, value_enum)]
        shell: Option<shells::WorkspaceShell>,
        /// Use an installed CLI wrapper rather than this executable.
        #[arg(long)]
        cli: Option<PathBuf>,
    },
    /// Install helpers and initialize the selected shell's user profile.
    ShellSetup {
        #[arg(long, value_enum)]
        shell: Option<shells::WorkspaceShell>,
        #[arg(long)]
        cli: Option<PathBuf>,
        /// Override the profile file, for example PowerShell's $PROFILE.
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        no_profile: bool,
    },
    #[command(name = "_wsl", hide = true)]
    Wsl {
        #[arg(last = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Inspect workspace metadata for the current directory, without acquiring a lease.
    Status {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    #[command(name = "_attach", hide = true)]
    Attach {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        pid: u32,
        #[arg(long)]
        child_pid: Option<u32>,
        #[arg(long)]
        inherited: Option<String>,
        #[arg(long, requires_all = ["profile", "state_home"])]
        thread_id: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        state_home: Option<PathBuf>,
    },
    #[command(name = "_resolve", hide = true)]
    Resolve {
        #[arg(long)]
        thread_id: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        state_home: PathBuf,
    },
    #[command(name = "_release", hide = true)]
    Release {
        #[arg(long)]
        id: String,
        #[arg(long)]
        lease: String,
        #[arg(long)]
        pid: u32,
    },
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(super) fn creation_source(cwd: &Path, selected: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = selected {
        return Ok(path.to_path_buf());
    }
    Ok(Registry::discover(cwd)?
        .map(|(_, record)| record.project.path)
        .unwrap_or_else(|| cwd.to_path_buf()))
}

fn existing(registry: &Registry, name: &str, project: Option<&Path>) -> Result<Record> {
    let record = registry.lookup(name, project)?;
    anyhow::ensure!(!record.deleted, "workspace was removed; recover explicitly instead of recreating a previous chat's directory");
    anyhow::ensure!(
        record.workspace.matches(&record.workspace.path),
        "workspace missing/replaced; explicit recovery required"
    );
    let (_, located) = Registry::discover(&record.workspace.path)?
        .context("workspace marker is missing; inspect/recover explicitly")?;
    anyhow::ensure!(located.id == record.id, "workspace marker identity changed");
    Ok(record)
}

fn summary(record: &Record) -> serde_json::Value {
    serde_json::json!({"id":record.id,"name":record.name,"project":record.project,"path":record.workspace.path,
        "backend":record.backend,"removed":record.deleted,"available":!record.deleted && record.workspace.matches(&record.workspace.path),
        "changes":if record.deleted {None} else {storage::dirty(record).ok()},
        "active":record.lease.as_ref().is_some_and(leases::active),"chats":record.chats,"native_cli_stores":record.native_homes,"pinned":record.pinned,
        "base_head":record.base_head,"lease":record.lease,"checkpoints":record.checkpoints})
}

fn run(record: &Record, arguments: &[OsString], interactive_shell: bool) -> Result<()> {
    let guard = acquire_for_directory(&record.workspace.path)?
        .context("workspace marker disappeared before launch")?;
    let mut command = if interactive_shell {
        let mut command =
            Command::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()));
        command.arg("-i");
        command
    } else {
        let executable = arguments
            .first()
            .cloned()
            .unwrap_or_else(|| "codex-fenced".into());
        let mut command = Command::new(executable);
        command.args(arguments.iter().skip(1));
        command
    };
    command.current_dir(&record.workspace.path);
    guard.configure_command(&mut command);
    // exec preserves terminal, signals, exact exit code, PID/start identity and
    // the lease. A failed exec drops the guard; a completed command is reaped
    // lazily by the next metadata operation without any background monitor.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("cannot execute workspace command")
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        bail!("writable Btrfs workspaces require Linux/WSL")
    }
}

pub fn execute(action: WorkspaceAction, entry_suffix: &[OsString]) -> Result<()> {
    // Shell installation and Windows dispatch do not inspect a Linux workspace.
    match &action {
        WorkspaceAction::ShellInit { shell, cli } => {
            println!(
                "{}",
                shells::render(
                    shell.unwrap_or_else(shells::detected),
                    &cli.clone().unwrap_or(std::env::current_exe()?),
                    entry_suffix,
                    None
                )?
            );
            return Ok(());
        }
        WorkspaceAction::ShellSetup {
            shell,
            cli,
            profile,
            directory,
            no_profile,
        } => {
            return print_json(&shells::setup(
                shell.unwrap_or_else(shells::detected),
                &cli.clone().unwrap_or(std::env::current_exe()?),
                entry_suffix,
                directory.as_deref(),
                profile.as_deref(),
                !no_profile,
            )?);
        }
        WorkspaceAction::Wsl { arguments } => return shells::dispatch_wsl(arguments),
        _ => {}
    }
    let registry = if matches!(
        &action,
        WorkspaceAction::Resolve { .. } | WorkspaceAction::Release { .. }
    ) {
        Registry::from_env()?
    } else {
        Registry::discover(&std::env::current_dir()?)?
            .map(|(registry, _)| registry)
            .unwrap_or(Registry::from_env()?)
    };
    match action {
        WorkspaceAction::Init { directory } => {
            registry.configure(&directory)?;
            eprintln!("Workspace storage: {}", directory.canonicalize()?.display());
            Ok(())
        }
        WorkspaceAction::Create {
            name,
            from,
            directory,
        } => {
            let source = creation_source(&std::env::current_dir()?, from.as_deref())?;
            let record = registry.create(&name, &source, directory.as_deref())?;
            eprintln!(
                "Workspace {}: {}",
                record.name,
                record.workspace.path.display()
            );
            println!("{}", record.workspace.path.display());
            Ok(())
        }
        WorkspaceAction::Recover {
            name,
            checkpoint,
            directory,
            apply,
        } => {
            if !apply {
                return print_json(&registry.recovery_preview(
                    &name,
                    &checkpoint,
                    directory.as_deref(),
                )?);
            }
            let record = registry.recover(&name, &checkpoint, directory.as_deref())?;
            eprintln!(
                "Recovered workspace {}: {} (existing work/history preserved)",
                record.name,
                record.workspace.path.display()
            );
            println!("{}", record.workspace.path.display());
            Ok(())
        }
        WorkspaceAction::Path {
            name,
            project,
            windows,
        } => {
            let path = existing(&registry, &name, project.as_deref())?
                .workspace
                .path;
            println!(
                "{}",
                if windows {
                    shells::windows_path(&path)?
                } else {
                    path.to_string_lossy().into_owned()
                }
            );
            Ok(())
        }
        WorkspaceAction::Run {
            name,
            project,
            command,
        } => run(
            &existing(&registry, &name, project.as_deref())?,
            &command,
            false,
        ),
        WorkspaceAction::Shell { name, project } => {
            run(&existing(&registry, &name, project.as_deref())?, &[], true)
        }
        WorkspaceAction::Ls {
            all,
            json,
            names,
            include_removed,
        } => {
            let cwd = std::env::current_dir()?;
            let selected = Registry::discover(&cwd)?
                .map(|(_, r)| r.project)
                .unwrap_or(Identity::read(&cwd)?);
            let records: Vec<_> = registry
                .records()?
                .into_iter()
                .filter(|r| {
                    (include_removed || !r.deleted)
                        && (all
                            || r.project.same(&selected)
                            || selected.path.starts_with(&r.project.path))
                })
                .collect();
            if json {
                return print_json(&records.iter().map(summary).collect::<Vec<_>>());
            }
            if names {
                for record in records {
                    println!("{}", record.name);
                }
            } else {
                println!("NAME\tSTATE\tCHANGES\tCHATS\tPATH");
                for record in records {
                    let status = summary(&record);
                    let state = if record.deleted {
                        "removed"
                    } else if !status["available"].as_bool().unwrap_or(false) {
                        "missing"
                    } else if status["active"].as_bool().unwrap_or(false) {
                        "active"
                    } else {
                        "idle"
                    };
                    let changed = match status["changes"].as_bool() {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => "unknown",
                    };
                    let chats = format!(
                        "{}{}",
                        record.chats.len(),
                        if record.native_homes.is_empty() {
                            ""
                        } else {
                            "+CLI"
                        }
                    );
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        record.name,
                        state,
                        changed,
                        chats,
                        record.workspace.path.display()
                    );
                }
            }
            Ok(())
        }
        WorkspaceAction::Diff { name, project } => {
            let record = existing(&registry, &name, project.as_deref())?;
            storage::independent_git(&record.workspace.path)?;
            let mut command = Command::new("git");
            command
                .args(["-c", "core.fsmonitor=false", "-C"])
                .arg(&record.workspace.path)
                .args(["diff", "--no-ext-diff", "--no-textconv"]);
            if let Some(head) = record.base_head {
                command.arg(head);
            }
            let status = command.arg("--").status()?;
            anyhow::ensure!(status.success(), "workspace Git diff failed");
            eprintln!("Untracked files (not included in the patch):");
            anyhow::ensure!(
                Command::new("git")
                    .arg("-C")
                    .arg(record.workspace.path)
                    .args(["ls-files", "--others", "--exclude-standard"])
                    .status()?
                    .success(),
                "cannot list untracked files"
            );
            Ok(())
        }
        WorkspaceAction::Rm {
            name,
            project,
            apply,
            discard_changes,
            detach_chats,
        } => print_json(&registry.remove_with(
            &name,
            project.as_deref(),
            apply,
            discard_changes,
            detach_chats,
            &storage::Btrfs,
        )?),
        WorkspaceAction::Pin {
            name,
            project,
            unpin,
        } => {
            let _lock = storage::lock(&registry)?;
            let mut record = registry.lookup(&name, project.as_deref())?;
            anyhow::ensure!(!record.deleted, "removed workspace cannot be pinned");
            record.pinned = !unpin;
            registry.save(&record)?;
            print_json(
                &serde_json::json!({"name":record.name,"pinned":record.pinned,"path":record.workspace.path}),
            )
        }
        WorkspaceAction::Status { path } => {
            print_json(&Registry::discover(&path)?.map(|(_, r)| summary(&r)))
        }
        WorkspaceAction::Attach {
            path,
            pid,
            child_pid,
            inherited,
            thread_id,
            profile,
            state_home,
        } => {
            let Some((actual, _)) = Registry::discover(&path)? else {
                return print_json(&serde_json::Value::Null);
            };
            let chat = thread_id
                .map(|id| {
                    Chat::new(
                        id,
                        profile.unwrap_or_default(),
                        state_home.as_deref().unwrap_or(Path::new(".")),
                    )
                })
                .transpose()?;
            let attachment = actual.attach(&path, pid, inherited.as_deref(), chat)?;
            if let (Some(value), Some(child)) = (&attachment, child_pid) {
                let guard = WorkspaceLease {
                    registry: actual.clone(),
                    attachment: leases::Attachment {
                        id: value.id.clone(),
                        name: value.name.clone(),
                        project: value.project.clone(),
                        workspace: value.workspace.clone(),
                        registry: value.registry.clone(),
                        lease: value.lease.clone(),
                        owned: false,
                    },
                };
                guard.track_child(child)?;
            }
            print_json(&attachment)
        }
        WorkspaceAction::Resolve {
            thread_id,
            profile,
            state_home,
        } => {
            let chat = Chat::new(thread_id, profile, &state_home)?;
            let record = registry.chat(&chat)?;
            let value = record.as_ref().map(|record| {
                let cwd = record
                    .chats
                    .iter()
                    .find(|value| value.same(&chat))
                    .and_then(|value| value.cwd.as_ref());
                serde_json::json!({"record": record, "cwd": cwd})
            });
            print_json(&value)
        }
        WorkspaceAction::Release { id, lease, pid } => {
            registry.release(&id, &lease, pid)?;
            print_json(&serde_json::json!({"released":true}))
        }
        WorkspaceAction::ShellInit { .. }
        | WorkspaceAction::ShellSetup { .. }
        | WorkspaceAction::Wsl { .. } => unreachable!("handled before workspace discovery"),
    }
}
