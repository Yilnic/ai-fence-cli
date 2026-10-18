//! Read-only, unprivileged Linux Btrfs child discovery. Kernel UAPI layouts
//! come from linux/btrfs.h; no shell parsing or full directory-tree walk.
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub(crate) fn descendants(root: &Path) -> Result<Vec<PathBuf>> {
    use std::collections::{HashSet, VecDeque};
    use std::ffi::OsString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct RootReference {
        tree_id: u64,
        directory_id: u64,
    }
    #[repr(C)]
    struct RootReferences {
        minimum_tree_id: u64,
        references: [RootReference; 255],
        count: u8,
        padding: [u8; 7],
    }
    #[repr(C)]
    struct Lookup {
        directory_id: u64,
        tree_id: u64,
        name: [u8; 256],
        path: [u8; 3824],
    }
    const _: () = assert!(std::mem::size_of::<RootReferences>() == 4096);
    const _: () = assert!(std::mem::size_of::<Lookup>() == 4096);
    // _IOWR(0x94, command, 4096) on Linux architectures supported by the CLI.
    const ROOT_REFERENCES: libc::c_ulong = 0xd000_943d;
    const LOOKUP_USER: libc::c_ulong = 0xd000_943e;
    fn terminated(bytes: &[u8]) -> Result<OsString> {
        let end = bytes
            .iter()
            .position(|&byte| byte == 0)
            .context("unterminated Btrfs path")?;
        Ok(OsString::from_vec(bytes[..end].to_vec()))
    }
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while let Some(parent) = queue.pop_front() {
        let file = std::fs::File::open(&parent)?;
        let mut references = RootReferences {
            minimum_tree_id: 0,
            references: [RootReference::default(); 255],
            count: 0,
            padding: [0; 7],
        };
        loop {
            let previous = references.minimum_tree_id;
            references.count = 0;
            // SAFETY: repr(C) buffer matches the 4096-byte kernel UAPI. The
            // descriptor remains open and the ioctl writes only that buffer.
            let code = unsafe { libc::ioctl(file.as_raw_fd(), ROOT_REFERENCES, &mut references) };
            if code < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EOVERFLOW) {
                    return Err(error).context(
                        "cannot enumerate Btrfs children without privileges; Linux 4.18+ required",
                    );
                }
            }
            if references.count == 0 {
                break;
            }
            for reference in references.references.iter().take(references.count as usize) {
                if !seen.insert(reference.tree_id) {
                    continue;
                }
                let mut lookup = Lookup {
                    directory_id: reference.directory_id,
                    tree_id: reference.tree_id,
                    name: [0; 256],
                    path: [0; 3824],
                };
                // SAFETY: same fixed-size UAPI/descriptor guarantees as above.
                if unsafe { libc::ioctl(file.as_raw_fd(), LOOKUP_USER, &mut lookup) } < 0 {
                    return Err(std::io::Error::last_os_error()).context(
                        "cannot resolve Btrfs child path; refusing an incomplete recursive set",
                    );
                }
                let relative =
                    PathBuf::from(terminated(&lookup.path)?).join(terminated(&lookup.name)?);
                if relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    bail!("Btrfs returned an unsafe child path");
                }
                let child = parent.join(relative);
                let canonical = super::canonical_directory(&child)?;
                if !canonical.starts_with(root) || canonical == root {
                    bail!("Btrfs child escaped the approved root");
                }
                queue.push_back(canonical.clone());
                result.push(canonical);
            }
            if references.minimum_tree_id <= previous {
                bail!("Btrfs child enumeration made no progress");
            }
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn descendants(_: &Path) -> Result<Vec<PathBuf>> {
    bail!("recursive Btrfs checkpoints require Linux")
}

#[cfg(target_os = "linux")]
pub(crate) fn uuid(root: &Path) -> Result<String> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Time {
        seconds: u64,
        nanoseconds: u32,
        padding: u32,
    }
    #[repr(C)]
    struct Info {
        tree_id: u64,
        name: [u8; 256],
        parent_id: u64,
        directory_id: u64,
        generation: u64,
        flags: u64,
        uuid: [u8; 16],
        parent_uuid: [u8; 16],
        received_uuid: [u8; 16],
        transactions: [u64; 4],
        times: [Time; 4],
        reserved: [u64; 8],
    }
    const _: () = assert!(std::mem::size_of::<Info>() == 504);
    let file = std::fs::File::open(root)?;
    let mut info = std::mem::MaybeUninit::<Info>::zeroed();
    // SAFETY: _IOR(0x94, 60, 504) writes the exact repr(C) Linux UAPI struct.
    if unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            0x81f8_943c as libc::c_ulong,
            info.as_mut_ptr(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error())
            .context("cannot read Btrfs subvolume identity; Linux 4.18+ required");
    }
    // SAFETY: the ioctl succeeded and initialized the output struct.
    let info = unsafe { info.assume_init() };
    if info.tree_id == 5 {
        bail!("refusing to checkpoint the Btrfs filesystem root");
    }
    Ok(uuid::Uuid::from_bytes(info.uuid).to_string())
}

#[cfg(target_os = "linux")]
pub(crate) fn filesystem_id(path: &Path) -> Result<[u8; 16]> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct FilesystemInfo {
        max_id: u64,
        devices: u64,
        fsid: [u8; 16],
        nodesize: u32,
        sectorsize: u32,
        clone_alignment: u32,
        checksum_type: u16,
        checksum_size: u16,
        flags: u64,
        generation: u64,
        metadata_uuid: [u8; 16],
        reserved: [u8; 944],
    }
    const _: () = assert!(std::mem::size_of::<FilesystemInfo>() == 1024);
    let file = std::fs::File::open(path)?;
    let mut info = std::mem::MaybeUninit::<FilesystemInfo>::zeroed();
    // SAFETY: _IOR(0x94, 31, 1024) writes exactly this repr(C) UAPI buffer.
    if unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            0x8400_941f as libc::c_ulong,
            info.as_mut_ptr(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot read Btrfs filesystem ID for {}", path.display()));
    }
    // SAFETY: a successful ioctl initialized the output.
    Ok(unsafe { info.assume_init() }.fsid)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn filesystem_id(_: &Path) -> Result<[u8; 16]> {
    bail!("Btrfs workspace checkpoints require Linux")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn uuid(_: &Path) -> Result<String> {
    bail!("Btrfs workspaces require Linux/WSL")
}
