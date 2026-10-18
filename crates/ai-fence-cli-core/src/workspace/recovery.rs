//! Explicit recovery clones a selected checkpoint into a new writable identity.
//! Neither conversation history nor an existing workspace is restored in place.
use super::*;
use storage::{Backend, Source};

pub(super) fn original_project(path: &Path, source_uuid: &str) -> Result<Option<Identity>> {
    let marker = path.join(MARKER);
    if !marker.exists() {
        return Ok(None);
    }
    let marker: Marker = storage::read_json(&marker)?;
    anyhow::ensure!(
        marker.format == FORMAT && marker.registry.is_absolute(),
        "checkpoint contains an invalid named workspace marker"
    );
    let registry = Registry {
        root: marker.registry,
    };
    storage::owned_directory(&registry.root)?;
    let record = registry.read(&marker.id)?;
    anyhow::ensure!(
        record.workspace.subvolume_uuid.as_deref() == Some(source_uuid)
            && record.workspace.inode == 256,
        "checkpoint workspace marker does not match the saved source subvolume"
    );
    Ok(Some(record.project))
}

pub(super) fn source(manifest: &Path, backend: &dyn Backend) -> Result<Source> {
    let checkpoint: crate::workspace_snapshot::Checkpoint = storage::read_json(manifest)?;
    anyhow::ensure!(
        checkpoint.format == "ai-fence-workspace-snapshot-set-v1"
            && checkpoint.complete
            && checkpoint.read_only,
        "recovery requires a complete read-only checkpoint set"
    );
    let id = checkpoint
        .id
        .strip_prefix("af-")
        .context("invalid checkpoint identity")?;
    uuid::Uuid::parse_str(id).context("invalid checkpoint identity")?;
    anyhow::ensure!(checkpoint.members.len() == 1, "nested checkpoint reconstruction needs deliberate handling; select a complete single-member checkpoint");
    let parent = manifest
        .parent()
        .context("checkpoint manifest needs a parent")?
        .canonicalize()?;
    anyhow::ensure!(
        checkpoint.path.canonicalize()? == parent,
        "checkpoint manifest location changed"
    );
    let member = &checkpoint.members[0];
    anyhow::ensure!(
        member.source_relative_path.as_os_str().is_empty()
            && member.snapshot_name.starts_with("member-")
            && member.snapshot_name[7..]
                .bytes()
                .all(|value| value.is_ascii_digit())
            && member.snapshot_name.len() >= 11,
        "checkpoint member path is unsafe or is not the project root"
    );
    let path = parent.join(&member.snapshot_name);
    storage::owned_directory(&path)?;
    let snapshot = backend.identity(&path)?;
    anyhow::ensure!(
        snapshot.subvolume_uuid.is_some() && snapshot.subvolume_uuid == member.snapshot_uuid,
        "checkpoint subvolume identity changed"
    );
    anyhow::ensure!(
        backend.read_only(&path)?,
        "selected checkpoint is no longer read-only"
    );
    storage::independent_git(&path)?;
    let mut project = if checkpoint.root.exists() {
        let project = backend.identity(&checkpoint.root)?;
        anyhow::ensure!(
            project.subvolume_uuid.as_deref() == Some(member.source_uuid.as_str()),
            "original project was replaced; inspect checkpoint provenance before recovery"
        );
        project
    } else {
        uuid::Uuid::parse_str(&member.source_uuid).context("invalid saved project identity")?;
        Identity {
            path: checkpoint.root.clone(),
            device: snapshot.device,
            inode: 256,
            subvolume_uuid: Some(member.source_uuid.clone()),
        }
    };
    if let Some(original) = original_project(&path, &member.source_uuid)? {
        if original.path.exists() {
            anyhow::ensure!(
                original.same(&backend.identity(&original.path)?),
                "original project identity changed; inspect checkpoint provenance before recovery"
            );
        }
        project = original;
    }
    validate_project_root(&project, dirs::home_dir().as_deref())?;
    anyhow::ensure!(
        project.inode == 256,
        "checkpoint does not identify an approved Btrfs project subvolume root"
    );
    Ok(Source {
        path,
        logical_root: project.path.clone(),
        project,
        checkpoint: Some(CheckpointReference {
            id: checkpoint.id,
            manifest: manifest.canonicalize()?,
        }),
    })
}

impl Registry {
    pub fn recovery_preview(
        &self,
        name: &str,
        manifest: &Path,
        directory: Option<&Path>,
    ) -> Result<serde_json::Value> {
        storage::valid_name(name)?;
        let source = source(manifest, &storage::Btrfs)?;
        let directory = self.directory_for(&source.project, directory)?;
        let existing = directory
            .ancestors()
            .find(|path| path.is_dir())
            .context("workspace destination has no available filesystem")?;
        storage::Btrfs.validate(&source.path, existing)?;
        let destination = directory
            .join(storage::project_key(&source.project))
            .join(name);
        anyhow::ensure!(
            !destination.exists(),
            "recovery destination already exists; select a new name"
        );
        Ok(
            serde_json::json!({"preview":true,"name":name,"project":source.project,"source":source.path,
            "workspace":destination,"checkpoint":source.checkpoint,"preserve_existing_workspaces":true,
            "conversation":"start/fork a thread in the new workspace; original thread/filesystem references are unchanged"}),
        )
    }

    pub fn recover(&self, name: &str, manifest: &Path, directory: Option<&Path>) -> Result<Record> {
        let source = source(manifest, &storage::Btrfs)?;
        self.create_source_with(name, source, directory, &storage::Btrfs)
    }
}
