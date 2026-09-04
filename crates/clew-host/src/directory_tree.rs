use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
};

use clew_core::{ControlModelError, ControllerId, DeviceId, ReadPolicy, SiteId, TransferId};
use clew_transport::{
    DirectoryConflictPolicy, DirectoryTreeEntry, DirectoryTreeError, DirectoryTreeManifest,
    FileTransferDirection, MAX_DIRECTORY_DEPTH, MAX_DIRECTORY_FILE_BYTES,
    MAX_DIRECTORY_TOTAL_BYTES, MAX_DIRECTORY_TREE_ENTRIES,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedDirectoryTree {
    canonical_root: PathBuf,
    pub entries: Vec<DirectoryTreeEntry>,
    pub total_file_bytes: u64,
}

impl ScannedDirectoryTree {
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    #[allow(clippy::too_many_arguments)]
    pub fn into_manifest(
        self,
        transfer_id: TransferId,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        direction: FileTransferDirection,
        device_root: impl Into<String>,
        device_conflict_policy: Option<DirectoryConflictPolicy>,
    ) -> Result<DirectoryTreeManifest, DirectoryTreeScanError> {
        Ok(DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            direction,
            device_root,
            self.entries,
            device_conflict_policy,
        )?)
    }
}

pub fn scan_authorized_directory_tree(
    policy: &ReadPolicy,
    root: &Path,
) -> Result<ScannedDirectoryTree, DirectoryTreeScanError> {
    policy.validate()?;
    if !root.is_absolute() {
        return Err(DirectoryTreeScanError::InvalidRoot);
    }
    let root_metadata = fs::symlink_metadata(root)?;
    if entry_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(DirectoryTreeScanError::InvalidRoot);
    }
    let canonical = fs::canonicalize(root)?;
    let allowed = policy.roots.iter().any(|allowed_root| {
        fs::canonicalize(allowed_root)
            .map(|allowed_root| canonical.starts_with(allowed_root))
            .unwrap_or(false)
    });
    if !allowed {
        return Err(DirectoryTreeScanError::OutsideAllowedRoots);
    }
    scan_directory_tree(&canonical)
}

/// Bounded deterministic local directory scan.
///
/// This function performs no authorization by itself; Controller-local source paths may use it
/// after their own local validation. Device-side callers should use `scan_authorized_directory_tree`.
pub fn scan_directory_tree(root: &Path) -> Result<ScannedDirectoryTree, DirectoryTreeScanError> {
    if !root.is_absolute() {
        return Err(DirectoryTreeScanError::InvalidRoot);
    }
    let root_metadata = fs::symlink_metadata(root)?;
    if entry_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(DirectoryTreeScanError::InvalidRoot);
    }
    let canonical_root = fs::canonicalize(root)?;
    let mut queue = VecDeque::from([(canonical_root.clone(), String::new(), 0_usize)]);
    let mut visited = BTreeSet::from([canonical_root.clone()]);
    let mut entries = Vec::new();
    let mut total_file_bytes = 0_u64;

    while let Some((directory, prefix, depth)) = queue.pop_front() {
        let mut children = fs::read_dir(&directory)?
            .map(|entry| {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| DirectoryTreeScanError::NonUtf8Path)?;
                Ok((name, entry.path()))
            })
            .collect::<Result<Vec<_>, DirectoryTreeScanError>>()?;
        children.sort_by(|left, right| left.0.cmp(&right.0));

        for (name, path) in children {
            if entries.len() >= MAX_DIRECTORY_TREE_ENTRIES {
                return Err(DirectoryTreeScanError::TooManyEntries(
                    entries.len().saturating_add(1),
                ));
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = fs::symlink_metadata(&path)?;
            if entry_is_link_or_reparse(&metadata) {
                return Err(DirectoryTreeScanError::UnsupportedEntry(relative));
            }
            if metadata.is_dir() {
                let child_depth = depth.saturating_add(1);
                if child_depth > MAX_DIRECTORY_DEPTH {
                    return Err(DirectoryTreeScanError::DepthExceeded(child_depth));
                }
                let canonical_child = fs::canonicalize(&path)?;
                if !canonical_child.starts_with(&canonical_root)
                    || !visited.insert(canonical_child.clone())
                {
                    return Err(DirectoryTreeScanError::DirectoryEscapeOrCycle(relative));
                }
                entries.push(DirectoryTreeEntry::directory(relative.clone())?);
                queue.push_back((canonical_child, relative, child_depth));
            } else if metadata.is_file() {
                let (size, sha256) = hash_bounded_file(&path)?;
                total_file_bytes = total_file_bytes
                    .checked_add(size)
                    .ok_or(DirectoryTreeScanError::TreeTooLarge(u64::MAX))?;
                if total_file_bytes > MAX_DIRECTORY_TOTAL_BYTES {
                    return Err(DirectoryTreeScanError::TreeTooLarge(total_file_bytes));
                }
                entries.push(DirectoryTreeEntry::file(relative, size, sha256)?);
            } else {
                return Err(DirectoryTreeScanError::UnsupportedEntry(relative));
            }
        }
    }

    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(ScannedDirectoryTree {
        canonical_root,
        entries,
        total_file_bytes,
    })
}

fn hash_bounded_file(path: &Path) -> Result<(u64, String), DirectoryTreeScanError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(DirectoryTreeScanError::FileTooLarge(u64::MAX))?;
        if total > MAX_DIRECTORY_FILE_BYTES {
            return Err(DirectoryTreeScanError::FileTooLarge(total));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((total, digest_hex(hasher.finalize())))
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn entry_is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

#[derive(Debug, Error)]
pub enum DirectoryTreeScanError {
    #[error(
        "directory tree root must be an absolute regular directory without symlink/reparse indirection"
    )]
    InvalidRoot,
    #[error("directory tree root is outside the signed read roots")]
    OutsideAllowedRoots,
    #[error("directory tree contains a non-UTF-8 path")]
    NonUtf8Path,
    #[error("directory tree contains unsupported symlink/reparse/special entry: {0}")]
    UnsupportedEntry(String),
    #[error("directory traversal escaped the root or formed a canonical cycle: {0}")]
    DirectoryEscapeOrCycle(String),
    #[error("directory tree contains too many entries: {0}")]
    TooManyEntries(usize),
    #[error("directory tree depth exceeds the hard bound: {0}")]
    DepthExceeded(usize),
    #[error("directory file exceeds the per-file hard bound: {0} bytes")]
    FileTooLarge(u64),
    #[error("directory tree exceeds the total-byte hard bound: {0} bytes")]
    TreeTooLarge(u64),
    #[error(transparent)]
    Model(#[from] ControlModelError),
    #[error(transparent)]
    Manifest(#[from] DirectoryTreeError),
    #[error("directory tree I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_transport::DirectoryTreeEntryKind;
    use tempfile::tempdir;

    #[test]
    fn scanner_is_deterministic_bounded_and_manifest_does_not_leak_local_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("controller-private-root");
        fs::create_dir_all(root.join("z-empty")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("b.txt"), b"bbb").unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        let root = fs::canonicalize(root).unwrap();

        let scan = scan_directory_tree(&root).unwrap();
        assert_eq!(scan.total_file_bytes, 4);
        assert_eq!(
            scan.entries
                .iter()
                .map(|entry| (entry.relative_path.as_str(), entry.kind))
                .collect::<Vec<_>>(),
            vec![
                ("a.txt", DirectoryTreeEntryKind::File),
                ("src", DirectoryTreeEntryKind::Directory),
                ("src/b.txt", DirectoryTreeEntryKind::File),
                ("z-empty", DirectoryTreeEntryKind::Directory),
            ]
        );
        let manifest = scan
            .into_manifest(
                TransferId::new(),
                ControllerId::new(),
                SiteId::new(),
                DeviceId::new(),
                FileTransferDirection::ControllerToDevice,
                "D:/device/project",
                Some(DirectoryConflictPolicy::FailIfExists),
            )
            .unwrap();
        let encoded = manifest.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("controller-private-root"));
    }

    #[test]
    fn authorized_scan_rejects_outside_root_and_entry_overflow() {
        let temp = tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let policy =
            ReadPolicy::new(vec![allowed.to_string_lossy().into_owned()], 4096, 5_000).unwrap();
        assert!(matches!(
            scan_authorized_directory_tree(&policy, &outside),
            Err(DirectoryTreeScanError::OutsideAllowedRoots)
        ));

        for index in 0..=MAX_DIRECTORY_TREE_ENTRIES {
            fs::write(allowed.join(format!("file-{index:03}.txt")), b"x").unwrap();
        }
        assert!(matches!(
            scan_authorized_directory_tree(&policy, &allowed),
            Err(DirectoryTreeScanError::TooManyEntries(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_symlink_instead_of_following_or_skipping_it() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(temp.path().join("outside.txt"), b"secret").unwrap();
        symlink(temp.path().join("outside.txt"), root.join("link.txt")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        assert!(matches!(
            scan_directory_tree(&root),
            Err(DirectoryTreeScanError::UnsupportedEntry(path)) if path == "link.txt"
        ));
    }
}
