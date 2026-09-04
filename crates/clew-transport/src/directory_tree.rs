use std::collections::BTreeMap;

use clew_core::{ControllerId, DeviceId, SiteId, TransferId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{EMPTY_SHA256_HEX, FileTransferDirection, MAX_FILE_RESUME_PATH_BYTES};

pub const DIRECTORY_TREE_MANIFEST_VERSION: u32 = 1;
pub const MAX_DIRECTORY_TREE_MANIFEST_BYTES: usize = 48 * 1024;
pub const MAX_DIRECTORY_TREE_ENTRIES: usize = 256;
pub const MAX_DIRECTORY_RELATIVE_PATH_BYTES: usize = 1024;
pub const MAX_DIRECTORY_DEPTH: usize = 64;
pub const MAX_DIRECTORY_FILE_BYTES: u64 = 1_u64 << 40;
pub const MAX_DIRECTORY_TOTAL_BYTES: u64 = 4_u64 << 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryConflictPolicy {
    FailIfExists,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryTreeEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryTreeEntry {
    pub relative_path: String,
    pub kind: DirectoryTreeEntryKind,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl DirectoryTreeEntry {
    pub fn directory(relative_path: impl Into<String>) -> Result<Self, DirectoryTreeError> {
        let entry = Self {
            relative_path: relative_path.into(),
            kind: DirectoryTreeEntryKind::Directory,
            size: 0,
            sha256: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn file(
        relative_path: impl Into<String>,
        size: u64,
        sha256: impl Into<String>,
    ) -> Result<Self, DirectoryTreeError> {
        let entry = Self {
            relative_path: relative_path.into(),
            kind: DirectoryTreeEntryKind::File,
            size,
            sha256: Some(sha256.into()),
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn validate(&self) -> Result<(), DirectoryTreeError> {
        validate_relative_path(&self.relative_path)?;
        match self.kind {
            DirectoryTreeEntryKind::Directory => {
                if self.size != 0 || self.sha256.is_some() {
                    return Err(DirectoryTreeError::InvalidDirectoryEntry);
                }
            }
            DirectoryTreeEntryKind::File => {
                if self.size > MAX_DIRECTORY_FILE_BYTES {
                    return Err(DirectoryTreeError::FileTooLarge(self.size));
                }
                let sha256 = self
                    .sha256
                    .as_deref()
                    .ok_or(DirectoryTreeError::MissingFileSha256)?;
                validate_sha256(sha256)?;
                if self.size == 0 && sha256 != EMPTY_SHA256_HEX {
                    return Err(DirectoryTreeError::EmptyFileHashMismatch);
                }
            }
        }
        Ok(())
    }
}

/// Peer-visible bounded directory tree contract.
///
/// `device_root` is always the Device-side path. Controller-local source/destination paths are
/// deliberately absent and remain private Controller state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryTreeManifest {
    pub version: u32,
    pub transfer_id: TransferId,
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub device_id: DeviceId,
    pub direction: FileTransferDirection,
    pub device_root: String,
    pub entries: Vec<DirectoryTreeEntry>,
    pub total_file_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_conflict_policy: Option<DirectoryConflictPolicy>,
}

impl DirectoryTreeManifest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transfer_id: TransferId,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        direction: FileTransferDirection,
        device_root: impl Into<String>,
        mut entries: Vec<DirectoryTreeEntry>,
        device_conflict_policy: Option<DirectoryConflictPolicy>,
    ) -> Result<Self, DirectoryTreeError> {
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let total_file_bytes = checked_total_file_bytes(&entries)?;
        let manifest = Self {
            version: DIRECTORY_TREE_MANIFEST_VERSION,
            transfer_id,
            controller_id,
            site_id,
            device_id,
            direction,
            device_root: device_root.into(),
            entries,
            total_file_bytes,
            device_conflict_policy,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), DirectoryTreeError> {
        if self.version != DIRECTORY_TREE_MANIFEST_VERSION {
            return Err(DirectoryTreeError::UnsupportedVersion(self.version));
        }
        validate_device_root(&self.device_root)?;
        if self.entries.len() > MAX_DIRECTORY_TREE_ENTRIES {
            return Err(DirectoryTreeError::TooManyEntries(self.entries.len()));
        }
        match (self.direction, self.device_conflict_policy) {
            (
                FileTransferDirection::ControllerToDevice,
                Some(DirectoryConflictPolicy::FailIfExists),
            )
            | (FileTransferDirection::DeviceToController, None) => {}
            (FileTransferDirection::ControllerToDevice, None) => {
                return Err(DirectoryTreeError::MissingDeviceConflictPolicy);
            }
            (FileTransferDirection::DeviceToController, Some(_)) => {
                return Err(DirectoryTreeError::ControllerPrivateConflictPolicyLeaked);
            }
        }

        let mut by_path = BTreeMap::new();
        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            entry.validate()?;
            if by_path
                .insert(entry.relative_path.as_str(), entry.kind)
                .is_some()
            {
                return Err(DirectoryTreeError::DuplicatePath(
                    entry.relative_path.clone(),
                ));
            }
            if previous.is_some_and(|value| value >= entry.relative_path.as_str()) {
                return Err(DirectoryTreeError::EntriesNotStrictlySorted);
            }
            previous = Some(&entry.relative_path);
            if let Some(parent) = relative_parent(&entry.relative_path) {
                match by_path.get(parent) {
                    Some(DirectoryTreeEntryKind::Directory) => {}
                    Some(DirectoryTreeEntryKind::File) => {
                        return Err(DirectoryTreeError::ParentIsFile(parent.to_owned()));
                    }
                    None => return Err(DirectoryTreeError::MissingParent(parent.to_owned())),
                }
            }
        }
        let actual_total = checked_total_file_bytes(&self.entries)?;
        if actual_total != self.total_file_bytes {
            return Err(DirectoryTreeError::TotalBytesMismatch {
                declared: self.total_file_bytes,
                actual: actual_total,
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, DirectoryTreeError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_DIRECTORY_TREE_MANIFEST_BYTES {
            return Err(DirectoryTreeError::ManifestTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, DirectoryTreeError> {
        if encoded.len() > MAX_DIRECTORY_TREE_MANIFEST_BYTES {
            return Err(DirectoryTreeError::ManifestTooLarge(encoded.len()));
        }
        let manifest: Self = serde_json::from_slice(encoded)?;
        manifest.validate()?;
        Ok(manifest)
    }
}

fn validate_device_root(root: &str) -> Result<(), DirectoryTreeError> {
    if root.trim().is_empty() || root.len() > MAX_FILE_RESUME_PATH_BYTES || root.contains('\0') {
        return Err(DirectoryTreeError::InvalidDeviceRoot);
    }
    Ok(())
}

pub fn validate_relative_path(path: &str) -> Result<(), DirectoryTreeError> {
    if path.is_empty()
        || path.len() > MAX_DIRECTORY_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.contains('\0')
    {
        return Err(DirectoryTreeError::InvalidRelativePath(path.to_owned()));
    }
    let mut depth = 0_usize;
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(DirectoryTreeError::InvalidRelativePath(path.to_owned()));
        }
        depth += 1;
        if depth > MAX_DIRECTORY_DEPTH {
            return Err(DirectoryTreeError::DepthExceeded(depth));
        }
    }
    Ok(())
}

fn relative_parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn checked_total_file_bytes(entries: &[DirectoryTreeEntry]) -> Result<u64, DirectoryTreeError> {
    let mut total = 0_u64;
    for entry in entries {
        if entry.kind == DirectoryTreeEntryKind::File {
            if entry.size > MAX_DIRECTORY_FILE_BYTES {
                return Err(DirectoryTreeError::FileTooLarge(entry.size));
            }
            total = total
                .checked_add(entry.size)
                .ok_or(DirectoryTreeError::TotalBytesOverflow)?;
            if total > MAX_DIRECTORY_TOTAL_BYTES {
                return Err(DirectoryTreeError::TreeTooLarge(total));
            }
        }
    }
    Ok(total)
}

fn validate_sha256(value: &str) -> Result<(), DirectoryTreeError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DirectoryTreeError::InvalidSha256);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum DirectoryTreeError {
    #[error("unsupported directory tree manifest version {0}")]
    UnsupportedVersion(u32),
    #[error("directory tree device root is invalid or exceeds its hard bound")]
    InvalidDeviceRoot,
    #[error("directory relative path is non-canonical or unsafe: {0}")]
    InvalidRelativePath(String),
    #[error("directory relative path depth exceeds the hard bound: {0}")]
    DepthExceeded(usize),
    #[error("directory tree contains too many entries: {0}")]
    TooManyEntries(usize),
    #[error("directory entry must have size=0 and no SHA-256")]
    InvalidDirectoryEntry,
    #[error("directory file entry is missing SHA-256")]
    MissingFileSha256,
    #[error("directory file SHA-256 must be canonical lowercase hex")]
    InvalidSha256,
    #[error("empty file entry must use the SHA-256 of empty bytes")]
    EmptyFileHashMismatch,
    #[error("directory file exceeds the per-file hard bound: {0} bytes")]
    FileTooLarge(u64),
    #[error("directory tree total bytes overflow u64")]
    TotalBytesOverflow,
    #[error("directory tree exceeds the total-byte hard bound: {0} bytes")]
    TreeTooLarge(u64),
    #[error("directory manifest total bytes mismatch: declared {declared}, actual {actual}")]
    TotalBytesMismatch { declared: u64, actual: u64 },
    #[error("directory entries must be strictly sorted by canonical relative path")]
    EntriesNotStrictlySorted,
    #[error("directory tree contains duplicate path {0}")]
    DuplicatePath(String),
    #[error("directory entry parent is missing: {0}")]
    MissingParent(String),
    #[error("directory entry parent is a file: {0}")]
    ParentIsFile(String),
    #[error("Controller-to-device directory transfer requires explicit fail-on-conflict policy")]
    MissingDeviceConflictPolicy,
    #[error("Device-to-controller directory manifest leaked Controller-private conflict policy")]
    ControllerPrivateConflictPolicyLeaked,
    #[error("directory tree manifest exceeds {MAX_DIRECTORY_TREE_MANIFEST_BYTES} bytes: {0}")]
    ManifestTooLarge(usize),
    #[error("directory tree JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (TransferId, ControllerId, SiteId, DeviceId) {
        (
            TransferId::new(),
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
        )
    }

    #[test]
    fn directory_manifest_roundtrips_canonical_sorted_tree_without_local_paths() {
        let (transfer_id, controller_id, site_id, device_id) = ids();
        let entries = vec![
            DirectoryTreeEntry::file("src/lib.rs", 7, "11".repeat(32)).unwrap(),
            DirectoryTreeEntry::directory("src").unwrap(),
            DirectoryTreeEntry::file("empty.txt", 0, EMPTY_SHA256_HEX).unwrap(),
        ];
        let manifest = DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            "D:/device/project",
            entries,
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();
        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|entry| entry.relative_path.as_str())
                .collect::<Vec<_>>(),
            vec!["empty.txt", "src", "src/lib.rs"]
        );
        assert_eq!(manifest.total_file_bytes, 7);
        let encoded = manifest.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("controller-private"));
        assert_eq!(DirectoryTreeManifest::decode(&encoded).unwrap(), manifest);
    }

    #[test]
    fn directory_manifest_rejects_unsafe_paths_hierarchy_and_wrong_conflict_scope() {
        for invalid in [
            "/absolute",
            "../escape",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "C:/drive",
            "a/",
        ] {
            assert!(
                validate_relative_path(invalid).is_err(),
                "accepted {invalid}"
            );
        }
        let (transfer_id, controller_id, site_id, device_id) = ids();
        let mut manifest = DirectoryTreeManifest {
            version: DIRECTORY_TREE_MANIFEST_VERSION,
            transfer_id,
            controller_id,
            site_id,
            device_id,
            direction: FileTransferDirection::ControllerToDevice,
            device_root: "D:/device/project".into(),
            entries: vec![DirectoryTreeEntry::file("a/b.txt", 1, "22".repeat(32)).unwrap()],
            total_file_bytes: 1,
            device_conflict_policy: Some(DirectoryConflictPolicy::FailIfExists),
        };
        assert!(matches!(
            manifest.validate(),
            Err(DirectoryTreeError::MissingParent(_))
        ));

        manifest.entries = vec![
            DirectoryTreeEntry::file("a", 1, "22".repeat(32)).unwrap(),
            DirectoryTreeEntry::file("a/b.txt", 1, "33".repeat(32)).unwrap(),
        ];
        manifest.total_file_bytes = 2;
        assert!(matches!(
            manifest.validate(),
            Err(DirectoryTreeError::ParentIsFile(_))
        ));

        manifest.entries = vec![
            DirectoryTreeEntry::directory("z").unwrap(),
            DirectoryTreeEntry::directory("a").unwrap(),
        ];
        manifest.total_file_bytes = 0;
        assert!(matches!(
            manifest.validate(),
            Err(DirectoryTreeError::EntriesNotStrictlySorted)
        ));

        manifest.entries.clear();
        manifest.direction = FileTransferDirection::DeviceToController;
        assert!(matches!(
            manifest.validate(),
            Err(DirectoryTreeError::ControllerPrivateConflictPolicyLeaked)
        ));
        manifest.device_conflict_policy = None;
        manifest.validate().unwrap();
    }

    #[test]
    fn directory_manifest_enforces_entry_depth_size_total_and_encoded_bounds() {
        let too_deep = (0..=MAX_DIRECTORY_DEPTH)
            .map(|_| "x")
            .collect::<Vec<_>>()
            .join("/");
        assert!(matches!(
            validate_relative_path(&too_deep),
            Err(DirectoryTreeError::DepthExceeded(_))
        ));
        assert!(matches!(
            DirectoryTreeEntry::file("huge.bin", MAX_DIRECTORY_FILE_BYTES + 1, "11".repeat(32)),
            Err(DirectoryTreeError::FileTooLarge(_))
        ));

        let (transfer_id, controller_id, site_id, device_id) = ids();
        let too_many_entries = (0..=MAX_DIRECTORY_TREE_ENTRIES)
            .map(|index| {
                DirectoryTreeEntry::file(format!("entry-{index:03}.bin"), 1, "11".repeat(32))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            DirectoryTreeManifest::new(
                transfer_id,
                controller_id,
                site_id,
                device_id,
                FileTransferDirection::DeviceToController,
                "/device/project",
                too_many_entries,
                None,
            ),
            Err(DirectoryTreeError::TooManyEntries(_))
        ));

        let total_too_large = (0..5)
            .map(|index| {
                DirectoryTreeEntry::file(
                    format!("huge-{index}.bin"),
                    MAX_DIRECTORY_FILE_BYTES,
                    "22".repeat(32),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            DirectoryTreeManifest::new(
                TransferId::new(),
                ControllerId::new(),
                SiteId::new(),
                DeviceId::new(),
                FileTransferDirection::DeviceToController,
                "/device/project",
                total_too_large,
                None,
            ),
            Err(DirectoryTreeError::TreeTooLarge(_))
        ));

        let (transfer_id, controller_id, site_id, device_id) = ids();
        let entries = (0..MAX_DIRECTORY_TREE_ENTRIES)
            .map(|index| {
                DirectoryTreeEntry::file(
                    format!("file-{index:03}-{}.bin", "x".repeat(180)),
                    1,
                    "44".repeat(32),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let manifest = DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/project",
            entries,
            None,
        )
        .unwrap();
        assert!(matches!(
            manifest.encode(),
            Err(DirectoryTreeError::ManifestTooLarge(_))
        ));

        let oversized = vec![b' '; MAX_DIRECTORY_TREE_MANIFEST_BYTES + 1];
        assert!(matches!(
            DirectoryTreeManifest::decode(&oversized),
            Err(DirectoryTreeError::ManifestTooLarge(actual))
                if actual == MAX_DIRECTORY_TREE_MANIFEST_BYTES + 1
        ));
    }
}
