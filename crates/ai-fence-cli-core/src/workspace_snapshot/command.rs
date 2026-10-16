use super::{SnapshotConfig, SnapshotOutcome, SnapshotPolicy};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct SnapshotPaths {
    /// Explicit workspace subvolume root; never inferred from filesystem ancestors.
    #[arg(long)]
    pub root: PathBuf,
    /// Existing snapshot-store directory outside the workspace, on the same Btrfs filesystem.
    #[arg(long)]
    pub directory: PathBuf,
    /// Include every nested subvolume in a sequential manifest-backed set.
    #[arg(long)]
    pub recursive: bool,
}
impl From<SnapshotPaths> for SnapshotConfig {
    fn from(paths: SnapshotPaths) -> Self {
        Self {
            root: paths.root,
            directory: paths.directory,
            recursive: paths.recursive,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SnapshotAction {
    /// Inspect approved source identities and nested subvolumes without checkpointing.
    Inspect(SnapshotPaths),
    /// Inspect explicitly approved scope without creating a checkpoint.
    Status(SnapshotPaths),
    /// Create a read-only checkpoint set. Pause writers for cross-subvolume consistency.
    Create(SnapshotPaths),
    /// List completed and incomplete checkpoint manifests for this exact root.
    List(SnapshotPaths),
    /// Pin a checkpoint against pruning, or clear its pin explicitly.
    Pin {
        #[command(flatten)]
        paths: SnapshotPaths,
        id: String,
        #[arg(long)]
        unpin: bool,
    },
    /// Plan retention cleanup; --apply explicitly deletes only validated snapshot sets.
    Prune {
        #[command(flatten)]
        paths: SnapshotPaths,
        /// Maximum snapshot sets considered for deletion in one invocation.
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
        #[arg(long)]
        apply: bool,
    },
}

/// Shared native/standalone CLI contract. Callers render this value as JSON.
pub fn execute(action: SnapshotAction) -> Result<serde_json::Value> {
    let policy = SnapshotPolicy::from_env()?;
    Ok(match action {
        SnapshotAction::Inspect(paths) => super::inspect_scope(&paths.into())?,
        SnapshotAction::Status(paths) => serde_json::to_value(if policy == SnapshotPolicy::Off {
            SnapshotOutcome {
                status: "off".into(),
                reason: Some("disabled by AI_FENCE_SNAPSHOTS=off".into()),
                checkpoint: None,
            }
        } else {
            super::status(&paths.into())
        })?,
        SnapshotAction::Create(paths) => {
            if policy == SnapshotPolicy::Off {
                bail!("checkpoint creation disabled by AI_FENCE_SNAPSHOTS=off");
            }
            serde_json::to_value(super::create(&paths.into())?)?
        }
        SnapshotAction::List(paths) => serde_json::to_value(super::list(&paths.into())?)?,
        SnapshotAction::Pin { paths, id, unpin } => {
            serde_json::to_value(super::pin(&paths.into(), &id, !unpin)?)?
        }
        SnapshotAction::Prune {
            paths,
            apply,
            limit,
        } => {
            if apply && policy == SnapshotPolicy::Off {
                bail!("snapshot mutations disabled by AI_FENCE_SNAPSHOTS=off");
            }
            serde_json::to_value(super::prune_bounded(
                &paths.into(),
                apply,
                usize::from(limit),
            )?)?
        }
    })
}
