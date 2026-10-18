//! Persistent named writable workspaces, independent of agent accounts/history.
//! Metadata operations use one lock; process leases survive exec and PID reuse.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

mod command;
mod leases;
mod recovery;
pub mod shells;
mod storage;
pub use command::{execute, WorkspaceAction};
pub use leases::{acquire_for_directory, validate_cli_resume, WorkspaceLease};

const FORMAT: &str = "ai-fence-named-workspace-v1";
const MARKER: &str = ".ai-fence-workspace.json";
const LEASE_ENV: &str = "AI_FENCE_WORKSPACE_LEASE";
const REGISTRY_ENV: &str = "AI_FENCE_WORKSPACE_HOME";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub subvolume_uuid: Option<String>,
}

impl Identity {
    #[cfg(unix)]
    pub fn read(path: &Path) -> Result<Self> {
        let path = path.canonicalize().with_context(|| {
            format!(
                "workspace path missing: {}; recover explicitly instead of recreating it",
                path.display()
            )
        })?;
        let info = fs::metadata(&path)?;
        anyhow::ensure!(info.is_dir(), "workspace path must be a directory");
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (info.dev(), info.ino())
        };
        let subvolume_uuid = crate::workspace_snapshot::discovery::uuid(&path).ok();
        Ok(Self {
            path,
            device,
            inode,
            subvolume_uuid,
        })
    }

    #[cfg(not(unix))]
    pub fn read(_: &Path) -> Result<Self> {
        bail!("named Btrfs workspace identity requires Linux/WSL")
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.path.is_absolute()
                && !self
                    .path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir),
            "workspace identity path must be absolute and normalized"
        );
        if let Some(value) = &self.subvolume_uuid {
            uuid::Uuid::parse_str(value).context("invalid saved Btrfs identity")?;
        }
        Ok(())
    }

    pub fn same(&self, other: &Self) -> bool {
        match (&self.subvolume_uuid, &other.subvolume_uuid) {
            (Some(left), Some(right)) => left == right && self.inode == other.inode,
            (None, None) => self.device == other.device && self.inode == other.inode,
            _ => false,
        }
    }

    fn matches(&self, path: &Path) -> bool {
        Self::read(path).is_ok_and(|value| self.same(&value))
    }
}

/// Saved recovery identities may outlive their original path. Compare both paths
/// and physical identities so aliases cannot make a whole home into a project.
fn validate_project_root(project: &Identity, home: Option<&Path>) -> Result<()> {
    project.validate()?;
    anyhow::ensure!(
        project.path.parent().is_some(),
        "an entire home or filesystem root is not a project"
    );
    for protected in [Some(Path::new("/")), Some(Path::new("/home")), home]
        .into_iter()
        .flatten()
    {
        let identity = Identity::read(protected).ok();
        anyhow::ensure!(
            project.path != protected
                && !identity
                    .as_ref()
                    .is_some_and(|root| { project.path == root.path || project.same(root) }),
            "an entire home or filesystem root is not a project"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub owner: Process,
    pub children: Vec<Process>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub thread_id: String,
    pub profile: String,
    pub state_home: PathBuf,
    pub state_identity: Identity,
    pub cwd: Option<Identity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeHome {
    pub profile: String,
    pub state_home: Identity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointReference {
    pub id: String,
    pub manifest: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub format: String,
    pub id: String,
    pub name: String,
    pub project: Identity,
    pub logical_root: PathBuf,
    pub workspace: Identity,
    pub created_at: String,
    pub backend: String,
    pub base_head: Option<String>,
    pub pinned: bool,
    pub deleted: bool,
    pub chats: Vec<Chat>,
    /// Interactive native CLI pickers do not expose thread notifications.
    /// Protect their workspace without inventing a thread identity.
    pub native_homes: Vec<NativeHome>,
    pub checkpoints: Vec<CheckpointReference>,
    pub lease: Option<Lease>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    format: String,
    id: String,
    registry: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Registry {
    pub root: PathBuf,
}

impl Registry {
    pub fn from_env() -> Result<Self> {
        let root = match std::env::var_os(REGISTRY_ENV) {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot resolve workspace registry; set AI_FENCE_WORKSPACE_HOME")?
                .join(".ai-fence/workspace-registry"),
        };
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        Ok(Self { root })
    }

    fn records(&self) -> Result<Vec<Record>> {
        storage::records(self)
    }

    fn record_path(&self, id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).context("invalid workspace identity")?;
        Ok(self.root.join("records").join(format!("{id}.json")))
    }

    fn read(&self, id: &str) -> Result<Record> {
        let record: Record = storage::read_json(&self.record_path(id)?)?;
        anyhow::ensure!(
            record.format == FORMAT && record.id == id,
            "unsupported workspace metadata identity/version"
        );
        storage::valid_name(&record.name)?;
        record.project.validate()?;
        record.workspace.validate()?;
        for chat in &record.chats {
            chat.state_identity.validate()?;
            anyhow::ensure!(
                chat.state_home == chat.state_identity.path,
                "native state identity path changed"
            );
            if let Some(cwd) = &chat.cwd {
                cwd.validate()?;
            }
        }
        for usage in &record.native_homes {
            usage.state_home.validate()?;
        }
        Ok(record)
    }

    fn save(&self, record: &Record) -> Result<()> {
        storage::save_json(&self.record_path(&record.id)?, record)
    }

    fn lookup(&self, name: &str, project: Option<&Path>) -> Result<Record> {
        storage::valid_name(name)?;
        let context = project
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let identity = match Self::discover(&context)? {
            Some((_, record)) => record.project,
            None => Identity::read(&context)?,
        };
        let mut candidates: Vec<_> = self
            .records()?
            .into_iter()
            .filter(|record| record.name == name)
            .collect();
        if candidates.iter().any(|r| r.project.same(&identity)) {
            candidates.retain(|r| r.project.same(&identity));
        } else {
            candidates.retain(|r| identity.path.starts_with(&r.project.path));
            if let Some(depth) = candidates
                .iter()
                .map(|r| r.project.path.components().count())
                .max()
            {
                candidates.retain(|r| r.project.path.components().count() == depth);
            }
        }
        if candidates.iter().any(|r| !r.deleted) {
            candidates.retain(|r| !r.deleted);
        } else if let Some(last) = candidates
            .iter()
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .cloned()
        {
            candidates = vec![last];
        }
        match candidates.as_slice() {
            [record] => Ok(record.clone()),
            [] => bail!("workspace {name:?} not found for this project; use workspace ls --all or --project PATH"),
            _ => bail!("workspace name is ambiguous; select --project PATH"),
        }
    }

    /// A marker is a locator, never authority: verify its owned record and physical identity.
    pub fn discover(cwd: &Path) -> Result<Option<(Self, Record)>> {
        let physical = cwd.canonicalize()?;
        for root in physical.ancestors() {
            let path = root.join(MARKER);
            if fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let marker: Marker = storage::read_json(&path)?;
            anyhow::ensure!(
                marker.format == FORMAT,
                "unsupported workspace marker version"
            );
            anyhow::ensure!(
                marker.registry.is_absolute(),
                "workspace marker registry path must be absolute"
            );
            let registry = Self {
                root: marker.registry,
            };
            storage::owned_directory(&registry.root)?;
            let record = registry.read(&marker.id)?;
            anyhow::ensure!(!record.deleted, "workspace was removed; recover explicitly");
            anyhow::ensure!(
                record.workspace.matches(root),
                "workspace moved or physical identity changed; recover/adopt explicitly"
            );
            return Ok(Some((registry, record)));
        }
        // A lost marker must not silently turn a known workspace into an
        // unmanaged directory. Read only the configured registry; do not scan
        // other profiles, histories or arbitrary filesystem locations.
        let registry = Self::from_env()?;
        let records: Vec<_> = registry
            .records()?
            .into_iter()
            .filter(|record| !record.deleted)
            .collect();
        if records
            .iter()
            .any(|record| physical.starts_with(&record.workspace.path))
        {
            bail!("named workspace marker is missing or replaced; inspect/recover explicitly before launching");
        }
        if !records.is_empty() {
            for root in physical.ancestors() {
                if let Ok(identity) = Identity::read(root) {
                    if records
                        .iter()
                        .any(|record| record.workspace.same(&identity))
                    {
                        bail!("named workspace marker is missing at a filesystem alias; inspect/recover explicitly");
                    }
                }
            }
        }
        Ok(None)
    }
}

/// CLI entry used by both the full distribution and the public standalone helper.
pub fn entry(arguments: impl IntoIterator<Item = OsString>) -> Result<()> {
    use clap::Parser;
    #[derive(Parser)]
    #[command(
        name = "ai-fence-workspace",
        about = "Persistent named Btrfs workspaces for parallel agents"
    )]
    struct Cli {
        #[command(subcommand)]
        action: WorkspaceAction,
    }
    execute(Cli::parse_from(arguments).action, &[])
}

#[cfg(test)]
mod tests;
