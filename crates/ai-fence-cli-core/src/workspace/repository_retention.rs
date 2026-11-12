//! Consumer-owned generation pins, separate from recovery checkpoint pinning.
use super::*;
use repository::Generation;

#[derive(Serialize, Deserialize)]
struct Pin {
    id: String,
    generation_id: String,
    consumer: String,
    expires_at: chrono::DateTime<chrono::Utc>,
    reader: Option<Process>,
}

impl Registry {
    pub fn repository_pin(
        &self,
        repository: &str,
        generation: &str,
        id: &str,
        consumer: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
        active_reader: bool,
    ) -> Result<()> {
        uuid::Uuid::parse_str(id)?;
        anyhow::ensure!(
            !consumer.is_empty()
                && consumer.len() <= 200
                && !consumer.chars().any(char::is_control),
            "invalid source consumer"
        );
        anyhow::ensure!(
            expires_at > chrono::Utc::now()
                && expires_at <= chrono::Utc::now() + chrono::Duration::hours(4),
            "source pin needs a finite deadline within four hours"
        );
        let _lock = storage::lock(self)?;
        let value = self.repository_generation(repository, Some(generation))?;
        let root = self.repository(repository)?.directory.path.join("leases");
        storage::private_directory(&root)?;
        let path = root.join(format!("{id}.json"));
        if path.exists() {
            let mut saved: Pin = storage::read_json(&path)?;
            anyhow::ensure!(
                saved.id == id
                    && saved.generation_id == value.id
                    && saved.consumer == consumer
                    && saved.expires_at == expires_at,
                "source lease identity changed"
            );
            if active_reader {
                let current =
                    Process::read(std::process::id()).context("cannot identify source reader")?;
                anyhow::ensure!(
                    saved
                        .reader
                        .as_ref()
                        .is_none_or(|reader| !reader.alive() || reader == &current),
                    "source view has another active reader"
                );
                saved.reader = Some(current);
                storage::save_json(&path, &saved)?;
            }
            return Ok(());
        }
        storage::save_json(
            &path,
            &Pin {
                id: id.into(),
                generation_id: value.id,
                consumer: consumer.into(),
                expires_at,
                reader: if active_reader {
                    Some(
                        Process::read(std::process::id())
                            .context("cannot identify source reader")?,
                    )
                } else {
                    None
                },
            },
        )
    }

    pub fn repository_unpin(&self, repository: &str, id: &str, consumer: &str) -> Result<()> {
        uuid::Uuid::parse_str(id)?;
        let _lock = storage::lock(self)?;
        let path = self
            .repository(repository)?
            .directory
            .path
            .join("leases")
            .join(format!("{id}.json"));
        let saved: Pin = storage::read_json(&path)?;
        anyhow::ensure!(
            saved.id == id && saved.consumer == consumer,
            "source lease belongs to another consumer"
        );
        anyhow::ensure!(
            saved.reader.as_ref().is_none_or(|reader| !reader.alive()),
            "source view still has an active reader"
        );
        fs::remove_file(path)?;
        Ok(())
    }

    pub(super) fn repository_finish_read(
        &self,
        repository: &str,
        id: &str,
        consumer: &str,
    ) -> Result<()> {
        uuid::Uuid::parse_str(id)?;
        let _lock = storage::lock(self)?;
        let path = self
            .repository(repository)?
            .directory
            .path
            .join("leases")
            .join(format!("{id}.json"));
        let mut pin: Pin = storage::read_json(&path)?;
        anyhow::ensure!(
            pin.consumer == consumer && pin.reader == Process::read(std::process::id()),
            "source reader incarnation changed"
        );
        pin.reader = None;
        storage::save_json(&path, &pin)
    }

    /// Only immutable managed bases are eligible. Writable workspaces and all
    /// recovery checkpoints are outside this retention domain.
    pub fn repository_prune(
        &self,
        id: &str,
        keep: usize,
        minimum_age_seconds: i64,
        apply: bool,
    ) -> Result<serde_json::Value> {
        anyhow::ensure!(minimum_age_seconds >= 0, "invalid source retention age");
        let _lock = storage::lock(self)?;
        let record = self.repository(id)?;
        let bases = record.directory.path.join("bases");
        let mut generations = Vec::<Generation>::new();
        for entry in fs::read_dir(&bases)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let generation: Generation = storage::read_json(&path)?;
                anyhow::ensure!(
                    path == bases.join(format!("{}.json", generation.id))
                        && generation.snapshot.path == bases.join(&generation.id),
                    "generation manifest path changed"
                );
                generations.push(generation);
            }
        }
        generations.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        let mut protected = std::collections::HashSet::from([record.current.id]);
        protected.extend(
            generations
                .iter()
                .take(keep)
                .map(|generation| generation.id.clone()),
        );
        let leases = record.directory.path.join("leases");
        let now = chrono::Utc::now();
        if leases.exists() {
            storage::owned_directory(&leases)?;
            for entry in fs::read_dir(leases)? {
                let path = entry?.path();
                let pin: Pin = storage::read_json(&path)?;
                anyhow::ensure!(
                    path.file_name() == Some(std::ffi::OsStr::new(&format!("{}.json", pin.id))),
                    "source pin filename changed"
                );
                if pin.expires_at > now || pin.reader.as_ref().is_some_and(Process::alive) {
                    protected.insert(pin.generation_id);
                } else if apply {
                    fs::remove_file(path)?;
                }
            }
        }
        let mut eligible = Vec::new();
        for generation in generations {
            let created = chrono::DateTime::parse_from_rfc3339(&generation.created_at)?
                .with_timezone(&chrono::Utc);
            if protected.contains(&generation.id)
                || (now - created).num_seconds() < minimum_age_seconds
            {
                continue;
            }
            self.verify_generation(&generation)?;
            eligible.push(generation.id.clone());
            if apply {
                // Btrfs's unprivileged deletion path may require write access
                // to the victim inode. It is unsealed only after all pin and
                // identity checks under the store lock, immediately for deletion.
                let status = std::process::Command::new("btrfs")
                    .args(["property", "set", "-ts"])
                    .arg(&generation.snapshot.path)
                    .args(["ro", "false"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()?;
                anyhow::ensure!(
                    status.success(),
                    "cannot prepare owned unleased generation for deletion"
                );
                if let Err(error) = storage::Btrfs.delete(&generation.snapshot.path) {
                    let _ = std::process::Command::new("btrfs")
                        .args(["property", "set", "-ts"])
                        .arg(&generation.snapshot.path)
                        .args(["ro", "true"])
                        .status();
                    return Err(error);
                }
                fs::remove_file(bases.join(format!("{}.json", generation.id)))?;
            }
        }
        Ok(
            serde_json::json!({"repository_id":id,"generation_ids":eligible,"applied":apply,"writable_workspaces_preserved":true,"recovery_checkpoints_preserved":true}),
        )
    }
}
use storage::Backend;
