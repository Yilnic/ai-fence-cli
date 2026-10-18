use super::*;
use std::process::Command;

impl Process {
    #[cfg(target_os = "linux")]
    pub(super) fn read(pid: u32) -> Option<Self> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields: Vec<_> = text
            .get(text.rfind(')')? + 2..)?
            .split_whitespace()
            .collect();
        if matches!(fields.first().copied(), Some("Z" | "X")) {
            return None;
        }
        let start = format!(
            "{}:{}",
            fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()?
                .trim(),
            fields.get(19)?
        );
        Some(Self { pid, start })
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn read(_: u32) -> Option<Self> {
        None
    }

    pub(super) fn alive(&self) -> bool {
        Self::read(self.pid).as_ref() == Some(self)
    }
}

#[cfg(target_os = "linux")]
fn ancestor(parent: u32, mut child: u32) -> bool {
    for _ in 0..128 {
        if child == parent {
            return true;
        }
        if child <= 1 {
            return false;
        }
        let Some(text) = fs::read_to_string(format!("/proc/{child}/stat")).ok() else {
            return false;
        };
        let Some(index) = text.rfind(')') else {
            return false;
        };
        let Some(pid) = text[index + 2..]
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
        else {
            return false;
        };
        child = pid;
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn ancestor(_: u32, _: u32) -> bool {
    false
}

pub(super) fn active(lease: &Lease) -> bool {
    lease.owner.alive() || lease.children.iter().any(Process::alive)
}

#[derive(Serialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub project: Identity,
    pub workspace: Identity,
    pub registry: PathBuf,
    pub lease: String,
    pub owned: bool,
}

/// A guard for a manual `cd; codex-fenced` launch. Managed runs export a lease
/// token whose owning PID/start identity must be an ancestor of their child.
pub struct WorkspaceLease {
    pub(super) registry: Registry,
    pub(super) attachment: Attachment,
}

impl WorkspaceLease {
    pub fn register_native_home(&self, home: &Path, profile: &str) -> Result<()> {
        let state_home = Identity::read(home)?;
        let _lock = storage::lock(&self.registry)?;
        let mut record = self.registry.read(&self.attachment.id)?;
        if !record
            .native_homes
            .iter()
            .any(|value| value.profile == profile && value.state_home.same(&state_home))
        {
            record.native_homes.push(NativeHome {
                profile: profile.into(),
                state_home,
            });
            self.registry.save(&record)?;
        }
        Ok(())
    }

    pub fn configure_command(&self, command: &mut Command) {
        command
            .env(REGISTRY_ENV, &self.registry.root)
            .env(LEASE_ENV, &self.attachment.lease)
            .env("AI_FENCE_WORKSPACE_ID", &self.attachment.id)
            .env("AI_FENCE_WORKSPACE_NAME", &self.attachment.name);
    }

    pub fn track_child(&self, pid: u32) -> Result<()> {
        let Some(child) = Process::read(pid) else {
            return Ok(());
        };
        let _lock = storage::lock(&self.registry)?;
        let mut record = self.registry.read(&self.attachment.id)?;
        let lease = record
            .lease
            .as_mut()
            .context("workspace lease disappeared during launch")?;
        anyhow::ensure!(
            lease.id == self.attachment.lease,
            "workspace lease changed during launch"
        );
        anyhow::ensure!(
            ancestor(lease.owner.pid, pid) || !child.alive(),
            "tracked process is outside the owning workspace launch"
        );
        if !lease.children.contains(&child) {
            lease.children.push(child);
        }
        self.registry.save(&record)
    }

    pub fn metadata(&self) -> &Attachment {
        &self.attachment
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if self.attachment.owned {
            let _ = self.registry.release(
                &self.attachment.id,
                &self.attachment.lease,
                std::process::id(),
            );
        }
    }
}

pub fn acquire_for_directory(cwd: &Path) -> Result<Option<WorkspaceLease>> {
    let Some((registry, _)) = Registry::discover(cwd)? else {
        return Ok(None);
    };
    let attachment = registry
        .attach(
            cwd,
            std::process::id(),
            std::env::var(LEASE_ENV).ok().as_deref(),
            None,
        )?
        .context("named workspace disappeared during launch")?;
    Ok(Some(WorkspaceLease {
        registry,
        attachment,
    }))
}

/// Known desktop/CLI thread links must not be resumed from another workspace.
/// Interactive picker-created threads remain native; native-home usage protects
/// their workspace even when the parent cannot observe a thread identifier.
pub fn validate_cli_resume(
    cwd: &Path,
    state_home: &Path,
    profile: &str,
    thread_id: &str,
) -> Result<()> {
    let current = Registry::discover(cwd)?;
    let registry = current
        .as_ref()
        .map(|(registry, _)| registry.clone())
        .unwrap_or(Registry::from_env()?);
    if let Some(saved) = registry.chat(&Chat::new(thread_id.into(), profile.into(), state_home)?)? {
        anyhow::ensure!(current.is_some_and(|(_, record)| record.id == saved.id),
            "thread belongs to named workspace {:?}; enter it with fws {:?} before resuming (history is unchanged)", saved.name, saved.name);
    }
    Ok(())
}

impl Chat {
    pub fn new(thread_id: String, profile: String, state_home: &Path) -> Result<Self> {
        anyhow::ensure!(
            !thread_id.is_empty()
                && thread_id.len() <= 128
                && thread_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
            "invalid native thread identity"
        );
        let state_identity = Identity::read(state_home)?;
        Ok(Self {
            thread_id,
            profile,
            state_home: state_identity.path.clone(),
            state_identity,
            cwd: None,
        })
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        self.thread_id == other.thread_id
            && self.profile == other.profile
            && self.state_identity.same(&other.state_identity)
    }
}

impl Registry {
    /// Attach before an agent can edit files. Passing a PID never authorizes
    /// another process: bridge requests must come from that PID's descendants.
    pub fn attach(
        &self,
        cwd: &Path,
        pid: u32,
        inherited: Option<&str>,
        chat: Option<Chat>,
    ) -> Result<Option<Attachment>> {
        anyhow::ensure!(
            ancestor(pid, std::process::id()),
            "workspace lease owner must be the caller or its parent process"
        );
        let owner =
            Process::read(pid).context("workspace lease owner is not alive; Linux/WSL required")?;
        let Some((actual, initial)) = Self::discover(cwd)? else {
            return Ok(None);
        };
        anyhow::ensure!(
            actual.root.canonicalize()? == self.root.canonicalize()?,
            "workspace belongs to another registry"
        );
        let _lock = storage::lock(self)?;
        let mut record = self.read(&initial.id)?;
        anyhow::ensure!(
            !record.deleted && record.workspace.matches(&record.workspace.path),
            "workspace missing/replaced; explicit recovery required"
        );
        let owned;
        if let Some(existing) = record.lease.as_ref().filter(|value| active(value)) {
            let same_owner = existing.owner == owner;
            let borrowed = inherited == Some(existing.id.as_str())
                && existing.owner.alive()
                && ancestor(existing.owner.pid, pid);
            anyhow::ensure!(
                same_owner || borrowed,
                "workspace {:?} is active in another launch (PID {}); choose another workspace",
                record.name,
                existing.owner.pid
            );
            owned = false;
        } else {
            record.lease = Some(Lease {
                id: uuid::Uuid::new_v4().to_string(),
                owner,
                children: Vec::new(),
            });
            owned = true;
        }
        if let Some(mut chat) = chat {
            if let Some(previous) = self
                .records()?
                .into_iter()
                .find(|r| r.chats.iter().any(|c| c.same(&chat)))
            {
                anyhow::ensure!(previous.id == record.id && !previous.deleted, "thread belongs to another named workspace; resume its existing workspace instead of changing history");
            }
            chat.cwd = Some(Identity::read(cwd)?);
            record.chats.retain(|c| !c.same(&chat));
            record.chats.push(chat);
        }
        self.save(&record)?;
        Ok(Some(Attachment {
            id: record.id,
            name: record.name,
            project: record.project,
            workspace: record.workspace,
            registry: self.root.clone(),
            lease: record.lease.context("missing workspace lease")?.id,
            owned,
        }))
    }

    pub fn release(&self, id: &str, token: &str, pid: u32) -> Result<()> {
        anyhow::ensure!(
            ancestor(pid, std::process::id()),
            "only the owning launch can release its workspace"
        );
        let _lock = storage::lock(self)?;
        let mut record = self.read(id)?;
        if record
            .lease
            .as_ref()
            .is_some_and(|lease| lease.id == token && lease.owner.pid == pid)
        {
            record.lease = None;
            self.save(&record)?;
        }
        Ok(())
    }

    pub fn chat(&self, chat: &Chat) -> Result<Option<Record>> {
        let records: Vec<_> = self
            .records()?
            .into_iter()
            .filter(|r| r.chats.iter().any(|c| c.same(chat)))
            .collect();
        match records.as_slice() {
            [] => Ok(None),
            [record] => {
                anyhow::ensure!(
                    !record.deleted,
                    "saved named workspace was removed; explicit recovery is required"
                );
                anyhow::ensure!(
                    record.workspace.matches(&record.workspace.path),
                    "saved named workspace is missing/replaced; no fallback to another project"
                );
                let (actual, located) = Self::discover(&record.workspace.path)?
                    .context("saved workspace marker is missing; explicit recovery required")?;
                anyhow::ensure!(
                    located.id == record.id
                        && actual.root.canonicalize()? == self.root.canonicalize()?,
                    "saved workspace marker belongs to another registry/identity"
                );
                Ok(Some(record.clone()))
            }
            _ => bail!("native thread has conflicting workspace links; inspect workspace metadata"),
        }
    }
}
