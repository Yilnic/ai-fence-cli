//! Export the native Windows lifecycle installer without requiring Python/WSL.
use anyhow::{ensure, Result};
use std::path::{Path, PathBuf};

pub fn export_installer(directory: &Path) -> Result<PathBuf> {
    ensure!(
        !directory.exists() || directory.read_dir()?.next().is_none(),
        "choose a new or empty installer export directory"
    );
    std::fs::create_dir_all(directory.join("windows-proxy"))?;
    for (name, source) in [
        (
            "install-windows-proxy.ps1",
            include_str!("../../../scripts/install-windows-proxy.ps1"),
        ),
        (
            "windows-proxy/start-proxy.ps1",
            include_str!("../../../scripts/windows-proxy/start-proxy.ps1"),
        ),
        (
            "windows-proxy/proxy_service.py",
            include_str!("../../../scripts/windows-proxy/proxy_service.py"),
        ),
    ] {
        std::fs::write(directory.join(name), source)?;
    }
    Ok(directory.join("install-windows-proxy.ps1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_self_contained_installer_and_preserves_existing_files() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("installer with spaces");
        let installer = export_installer(&directory).unwrap();
        assert!(installer.is_file());
        assert!(directory.join("windows-proxy/start-proxy.ps1").is_file());
        assert!(directory.join("windows-proxy/proxy_service.py").is_file());
        let edited = "user-maintained installer";
        std::fs::write(&installer, edited).unwrap();
        assert!(export_installer(&directory).is_err());
        assert_eq!(std::fs::read_to_string(installer).unwrap(), edited);
    }
}
