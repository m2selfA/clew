use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
};

use clew_core::{ControlModelError, ControllerId, DeviceId, ReadPolicy, SiteId, TransferId};
use clew_transport::{
    DirectoryConflictPolicy, DirectoryTreeEntry, DirectoryTreeEntryKind, DirectoryTreeError,
    DirectoryTreeErrorCode, DirectoryTreeManifest, DirectoryTreeReply, DirectoryTreeRequest,
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

#[derive(Clone, Debug)]
pub struct HostDirectoryTreeService {
    policy: ReadPolicy,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    can_get: bool,
    can_put: bool,
}

impl HostDirectoryTreeService {
    pub fn new(
        policy: ReadPolicy,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        can_get: bool,
        can_put: bool,
    ) -> Result<Self, DirectoryTreeScanError> {
        policy.validate()?;
        Ok(Self {
            policy,
            controller_id,
            site_id,
            device_id,
            can_get,
            can_put,
        })
    }

    pub async fn execute(
        &self,
        request: DirectoryTreeRequest,
        allow_read: bool,
        allow_write: bool,
    ) -> DirectoryTreeReply {
        if request.validate().is_err() {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "invalid bounded directory tree request",
            );
        }
        let permitted = match &request {
            DirectoryTreeRequest::PreparePut { .. }
            | DirectoryTreeRequest::FinalizePut { .. }
            | DirectoryTreeRequest::CancelPut { .. } => self.can_put && allow_write,
            DirectoryTreeRequest::PrepareGet { .. } | DirectoryTreeRequest::FinalizeGet { .. } => {
                self.can_get && allow_read
            }
        };
        if !permitted || self.policy.roots.is_empty() {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Denied,
                "directory transfer is not permitted by this device grant",
            );
        }
        let service = self.clone();
        match tokio::task::spawn_blocking(move || service.execute_blocking(request)).await {
            Ok(reply) => reply,
            Err(_) => DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Io,
                "directory tree worker failed",
            ),
        }
    }

    fn execute_blocking(&self, request: DirectoryTreeRequest) -> DirectoryTreeReply {
        match request {
            DirectoryTreeRequest::PreparePut { manifest } => {
                if !self.manifest_scope_matches(&manifest) {
                    return self.scope_denied();
                }
                self.prepare_put(&manifest)
            }
            DirectoryTreeRequest::FinalizePut { manifest } => {
                if !self.manifest_scope_matches(&manifest) {
                    return self.scope_denied();
                }
                self.finalize_put(&manifest)
            }
            DirectoryTreeRequest::CancelPut { manifest } => {
                if !self.manifest_scope_matches(&manifest) {
                    return self.scope_denied();
                }
                self.cancel_put(&manifest)
            }
            DirectoryTreeRequest::PrepareGet { scope } => {
                if scope.controller_id != self.controller_id
                    || scope.site_id != self.site_id
                    || scope.device_id != self.device_id
                {
                    return self.scope_denied();
                }
                self.prepare_get(&scope)
            }
            DirectoryTreeRequest::FinalizeGet { manifest } => {
                if !self.manifest_scope_matches(&manifest) {
                    return self.scope_denied();
                }
                self.finalize_get(&manifest)
            }
        }
    }

    fn manifest_scope_matches(&self, manifest: &DirectoryTreeManifest) -> bool {
        manifest.controller_id == self.controller_id
            && manifest.site_id == self.site_id
            && manifest.device_id == self.device_id
    }

    fn scope_denied(&self) -> DirectoryTreeReply {
        DirectoryTreeReply::error(
            DirectoryTreeErrorCode::Denied,
            "directory tree scope does not match this Host",
        )
    }

    fn prepare_get(&self, scope: &clew_transport::DirectoryTreeGetScope) -> DirectoryTreeReply {
        let root = match self.get_root(&scope.device_root) {
            Ok(root) => root,
            Err(reply) => return reply,
        };
        let scan = match scan_authorized_directory_tree(&self.policy, &root) {
            Ok(scan) => scan,
            Err(error) => return directory_scan_reply(error),
        };
        match scan.into_manifest(
            scope.transfer_id,
            self.controller_id,
            self.site_id,
            self.device_id,
            FileTransferDirection::DeviceToController,
            scope.device_root.clone(),
            None,
        ) {
            Ok(manifest) => DirectoryTreeReply::Manifest { manifest },
            Err(error) => directory_scan_reply(error),
        }
    }

    fn finalize_get(&self, manifest: &DirectoryTreeManifest) -> DirectoryTreeReply {
        if manifest.validate().is_err()
            || manifest.direction != FileTransferDirection::DeviceToController
            || manifest.device_conflict_policy.is_some()
        {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "invalid directory Get manifest",
            );
        }
        let root = match self.get_root(&manifest.device_root) {
            Ok(root) => root,
            Err(reply) => return reply,
        };
        let scan = match scan_authorized_directory_tree(&self.policy, &root) {
            Ok(scan) => scan,
            Err(error) => return directory_scan_reply(error),
        };
        if scan.entries != manifest.entries || scan.total_file_bytes != manifest.total_file_bytes {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::HashMismatch,
                "device directory changed during transfer",
            );
        }
        DirectoryTreeReply::Verified {
            transfer_id: manifest.transfer_id,
            device_root: manifest.device_root.clone(),
        }
    }

    fn get_root(&self, device_root: &str) -> Result<PathBuf, DirectoryTreeReply> {
        let requested = PathBuf::from(device_root);
        if !requested.is_absolute() {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Denied,
                "device directory source must be absolute",
            ));
        }
        let metadata = fs::symlink_metadata(&requested).map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                DirectoryTreeErrorCode::NotFound
            } else {
                DirectoryTreeErrorCode::Io
            };
            DirectoryTreeReply::error(code, "device directory source metadata failed")
        })?;
        if entry_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Denied,
                "device directory source is not a safe regular directory",
            ));
        }
        fs::canonicalize(&requested).map_err(|_| {
            DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Io,
                "device directory source could not be canonicalized",
            )
        })
    }

    fn prepare_put(&self, manifest: &DirectoryTreeManifest) -> DirectoryTreeReply {
        let (final_root, staging_root) = match self.put_paths(manifest) {
            Ok(paths) => paths,
            Err(reply) => return reply,
        };
        match fs::symlink_metadata(&final_root) {
            Ok(_) => {
                return DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Conflict,
                    "directory destination already exists",
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Io,
                    "directory destination metadata failed",
                );
            }
        }
        if let Err(reply) = ensure_staging_root(&staging_root) {
            return reply;
        }
        for entry in &manifest.entries {
            if entry.kind != DirectoryTreeEntryKind::Directory {
                continue;
            }
            let path = join_relative(&staging_root, &entry.relative_path);
            if let Err(reply) = ensure_manifest_directory(&path) {
                return reply;
            }
        }
        if let Err(reply) = sync_parent_directory(&staging_root) {
            return reply;
        }
        let Some(staging_device_root) = staging_root.to_str().map(str::to_owned) else {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Io,
                "directory staging path is not valid UTF-8",
            );
        };
        DirectoryTreeReply::Prepared {
            transfer_id: manifest.transfer_id,
            staging_device_root,
        }
    }

    fn finalize_put(&self, manifest: &DirectoryTreeManifest) -> DirectoryTreeReply {
        let (final_root, staging_root) = match self.put_paths(manifest) {
            Ok(paths) => paths,
            Err(reply) => return reply,
        };
        let staging_metadata = fs::symlink_metadata(&staging_root);
        let final_metadata = fs::symlink_metadata(&final_root);
        let staging_exists = staging_metadata.is_ok();
        let final_exists = final_metadata.is_ok();
        if staging_metadata
            .as_ref()
            .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound)
            || final_metadata
                .as_ref()
                .is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound)
        {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Io,
                "directory finalize metadata failed",
            );
        }
        if !staging_exists && final_exists {
            return match verify_tree_matches(&final_root, manifest) {
                Ok(()) => DirectoryTreeReply::Completed {
                    transfer_id: manifest.transfer_id,
                    final_device_root: manifest.device_root.clone(),
                },
                Err(reply) => reply,
            };
        }
        if !staging_exists {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::NotFound,
                "directory staging tree was not found",
            );
        }
        if final_exists {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Conflict,
                "directory destination appeared before finalize",
            );
        }
        if let Err(reply) = verify_tree_matches(&staging_root, manifest) {
            return reply;
        }
        if fs::rename(&staging_root, &final_root).is_err() {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Io,
                "atomic directory finalize failed",
            );
        }
        if let Err(reply) = sync_parent_directory(&final_root) {
            return reply;
        }
        DirectoryTreeReply::Completed {
            transfer_id: manifest.transfer_id,
            final_device_root: manifest.device_root.clone(),
        }
    }

    fn cancel_put(&self, manifest: &DirectoryTreeManifest) -> DirectoryTreeReply {
        let (final_root, staging_root) = match self.put_paths(manifest) {
            Ok(paths) => paths,
            Err(reply) => return reply,
        };
        if fs::symlink_metadata(&final_root).is_ok() && fs::symlink_metadata(&staging_root).is_err()
        {
            return DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Conflict,
                "directory transfer is already finalized",
            );
        }
        match fs::symlink_metadata(&staging_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Io,
                    "directory staging metadata failed",
                );
            }
            Ok(metadata) => {
                if entry_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return DirectoryTreeReply::error(
                        DirectoryTreeErrorCode::Denied,
                        "directory staging root is not a safe regular directory",
                    );
                }
                if validate_safe_tree_for_removal(&staging_root).is_err()
                    || fs::remove_dir_all(&staging_root).is_err()
                {
                    return DirectoryTreeReply::error(
                        DirectoryTreeErrorCode::Io,
                        "directory staging cleanup failed",
                    );
                }
                if let Err(reply) = sync_parent_directory(&staging_root) {
                    return reply;
                }
            }
        }
        DirectoryTreeReply::Cancelled {
            transfer_id: manifest.transfer_id,
        }
    }

    fn put_paths(
        &self,
        manifest: &DirectoryTreeManifest,
    ) -> Result<(PathBuf, PathBuf), DirectoryTreeReply> {
        if manifest.validate().is_err()
            || manifest.direction != FileTransferDirection::ControllerToDevice
            || manifest.device_conflict_policy != Some(DirectoryConflictPolicy::FailIfExists)
        {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "invalid directory Put manifest",
            ));
        }
        let requested = PathBuf::from(&manifest.device_root);
        if !requested.is_absolute() {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Denied,
                "directory destination must be absolute",
            ));
        }
        let Some(std::path::Component::Normal(_)) = requested.components().next_back() else {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "directory destination must end in a normal name",
            ));
        };
        let Some(parent) = requested.parent() else {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "directory destination parent is invalid",
            ));
        };
        let parent = fs::canonicalize(parent).map_err(|_| {
            DirectoryTreeReply::error(
                DirectoryTreeErrorCode::NotFound,
                "directory destination parent was not found",
            )
        })?;
        if !self.policy.roots.iter().any(|root| {
            fs::canonicalize(root)
                .map(|root| parent.starts_with(root))
                .unwrap_or(false)
        }) {
            return Err(DirectoryTreeReply::error(
                DirectoryTreeErrorCode::Denied,
                "directory destination is outside signed roots",
            ));
        }
        let final_name = requested.file_name().ok_or_else(|| {
            DirectoryTreeReply::error(
                DirectoryTreeErrorCode::InvalidRequest,
                "invalid directory destination",
            )
        })?;
        let final_root = parent.join(final_name);
        let staging_root = parent.join(format!(".clew-dir-{}.part", manifest.transfer_id));
        Ok((final_root, staging_root))
    }
}

fn ensure_staging_root(path: &Path) -> Result<(), DirectoryTreeReply> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !entry_is_link_or_reparse(&metadata) && metadata.is_dir() => Ok(()),
        Ok(_) => Err(DirectoryTreeReply::error(
            DirectoryTreeErrorCode::Denied,
            "directory staging path is not a safe directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| {
                DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Io,
                    "directory staging root could not be created",
                )
            })
        }
        Err(_) => Err(DirectoryTreeReply::error(
            DirectoryTreeErrorCode::Io,
            "directory staging metadata failed",
        )),
    }
}

fn ensure_manifest_directory(path: &Path) -> Result<(), DirectoryTreeReply> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !entry_is_link_or_reparse(&metadata) && metadata.is_dir() => Ok(()),
        Ok(_) => Err(DirectoryTreeReply::error(
            DirectoryTreeErrorCode::Denied,
            "directory staging entry conflicts with a non-directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| {
                DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Io,
                    "directory staging entry could not be created",
                )
            })
        }
        Err(_) => Err(DirectoryTreeReply::error(
            DirectoryTreeErrorCode::Io,
            "directory staging entry metadata failed",
        )),
    }
}

fn join_relative(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

fn verify_tree_matches(
    root: &Path,
    manifest: &DirectoryTreeManifest,
) -> Result<(), DirectoryTreeReply> {
    let scan = scan_directory_tree(root).map_err(directory_scan_reply)?;
    if scan.entries != manifest.entries || scan.total_file_bytes != manifest.total_file_bytes {
        return Err(DirectoryTreeReply::error(
            DirectoryTreeErrorCode::HashMismatch,
            "directory staging tree does not match the signed manifest",
        ));
    }
    Ok(())
}

fn validate_safe_tree_for_removal(root: &Path) -> Result<(), DirectoryTreeScanError> {
    let canonical_root = fs::canonicalize(root)?;
    let mut queue = VecDeque::from([(canonical_root.clone(), 0_usize)]);
    let mut visited = BTreeSet::from([canonical_root.clone()]);
    let mut entries = 0_usize;
    while let Some((directory, depth)) = queue.pop_front() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            entries = entries.saturating_add(1);
            if entries > MAX_DIRECTORY_TREE_ENTRIES {
                return Err(DirectoryTreeScanError::TooManyEntries(entries));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if entry_is_link_or_reparse(&metadata) {
                return Err(DirectoryTreeScanError::UnsupportedEntry(
                    entry.file_name().to_string_lossy().into_owned(),
                ));
            }
            if metadata.is_dir() {
                let next_depth = depth.saturating_add(1);
                if next_depth > MAX_DIRECTORY_DEPTH {
                    return Err(DirectoryTreeScanError::DepthExceeded(next_depth));
                }
                let canonical = fs::canonicalize(entry.path())?;
                if !canonical.starts_with(&canonical_root) || !visited.insert(canonical.clone()) {
                    return Err(DirectoryTreeScanError::DirectoryEscapeOrCycle(
                        entry.file_name().to_string_lossy().into_owned(),
                    ));
                }
                queue.push_back((canonical, next_depth));
            } else if !metadata.is_file() {
                return Err(DirectoryTreeScanError::UnsupportedEntry(
                    entry.file_name().to_string_lossy().into_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn directory_scan_reply(error: DirectoryTreeScanError) -> DirectoryTreeReply {
    let message = error.to_string();
    match error {
        DirectoryTreeScanError::OutsideAllowedRoots => {
            DirectoryTreeReply::error(DirectoryTreeErrorCode::Denied, message)
        }
        DirectoryTreeScanError::Io(_) => {
            DirectoryTreeReply::error(DirectoryTreeErrorCode::Io, message)
        }
        _ => DirectoryTreeReply::error(DirectoryTreeErrorCode::InvalidRequest, message),
    }
}

fn sync_parent_directory(_path: &Path) -> Result<(), DirectoryTreeReply> {
    #[cfg(unix)]
    {
        let parent = _path.parent().ok_or_else(|| {
            DirectoryTreeReply::error(DirectoryTreeErrorCode::Io, "directory parent is invalid")
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                DirectoryTreeReply::error(
                    DirectoryTreeErrorCode::Io,
                    "directory parent sync failed",
                )
            })?;
    }
    Ok(())
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
    use clew_transport::{DirectoryTreeEntryKind, DirectoryTreeGetScope, file_sha256_hex};
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

    #[tokio::test]
    async fn host_directory_put_prepares_verifies_finalizes_replays_and_cancels() {
        let temp = tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let policy =
            ReadPolicy::new(vec![allowed.to_string_lossy().into_owned()], 49_152, 5_000).unwrap();
        let service = HostDirectoryTreeService::new(
            policy.clone(),
            controller_id,
            site_id,
            device_id,
            true,
            true,
        )
        .unwrap();
        let transfer_id = TransferId::new();
        let final_root = allowed.join("project");
        let manifest = DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            final_root.to_string_lossy(),
            vec![
                DirectoryTreeEntry::file("a.txt", 1, file_sha256_hex(b"a")).unwrap(),
                DirectoryTreeEntry::directory("src").unwrap(),
                DirectoryTreeEntry::file("src/b.txt", 3, file_sha256_hex(b"bbb")).unwrap(),
                DirectoryTreeEntry::directory("z-empty").unwrap(),
            ],
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();

        let prepared = service
            .execute(
                DirectoryTreeRequest::PreparePut {
                    manifest: manifest.clone(),
                },
                true,
                true,
            )
            .await;
        let DirectoryTreeReply::Prepared {
            staging_device_root,
            ..
        } = prepared
        else {
            panic!("expected directory staging preparation");
        };
        let staging = PathBuf::from(&staging_device_root);
        assert!(!final_root.exists());
        assert!(staging.join("src").is_dir());
        assert!(staging.join("z-empty").is_dir());
        fs::write(staging.join("a.txt"), b"a").unwrap();
        fs::write(staging.join("src/b.txt"), b"bbb").unwrap();

        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::FinalizePut {
                        manifest: manifest.clone(),
                    },
                    true,
                    true,
                )
                .await,
            DirectoryTreeReply::Completed { transfer_id: actual, .. } if actual == transfer_id
        ));
        assert!(final_root.join("a.txt").is_file());
        assert!(final_root.join("src/b.txt").is_file());
        assert!(final_root.join("z-empty").is_dir());
        assert!(!staging.exists());

        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::FinalizePut {
                        manifest: manifest.clone(),
                    },
                    true,
                    true,
                )
                .await,
            DirectoryTreeReply::Completed { transfer_id: actual, .. } if actual == transfer_id
        ));

        let cancel_id = TransferId::new();
        let cancel_root = allowed.join("cancelled");
        let cancel_manifest = DirectoryTreeManifest::new(
            cancel_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            cancel_root.to_string_lossy(),
            vec![DirectoryTreeEntry::directory("empty").unwrap()],
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();
        let DirectoryTreeReply::Prepared {
            staging_device_root,
            ..
        } = service
            .execute(
                DirectoryTreeRequest::PreparePut {
                    manifest: cancel_manifest.clone(),
                },
                true,
                true,
            )
            .await
        else {
            panic!("expected cancellable directory staging");
        };
        let cancel_staging = PathBuf::from(staging_device_root);
        assert!(cancel_staging.is_dir());
        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::CancelPut {
                        manifest: cancel_manifest,
                    },
                    true,
                    true,
                )
                .await,
            DirectoryTreeReply::Cancelled { transfer_id: actual } if actual == cancel_id
        ));
        assert!(!cancel_staging.exists());
        assert!(!cancel_root.exists());
    }

    #[tokio::test]
    async fn host_directory_put_fails_closed_on_grant_scope_or_tree_tamper() {
        let temp = tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let policy =
            ReadPolicy::new(vec![allowed.to_string_lossy().into_owned()], 49_152, 5_000).unwrap();
        let service = HostDirectoryTreeService::new(
            policy.clone(),
            controller_id,
            site_id,
            device_id,
            true,
            true,
        )
        .unwrap();
        let manifest = DirectoryTreeManifest::new(
            TransferId::new(),
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            allowed.join("tampered").to_string_lossy(),
            vec![DirectoryTreeEntry::file("a.txt", 1, file_sha256_hex(b"a")).unwrap()],
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();

        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::PreparePut {
                        manifest: manifest.clone(),
                    },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::Denied
        ));
        let readonly =
            HostDirectoryTreeService::new(policy, controller_id, site_id, device_id, true, false)
                .unwrap();
        assert!(matches!(
            readonly
                .execute(
                    DirectoryTreeRequest::PreparePut {
                        manifest: manifest.clone(),
                    },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::Denied
        ));

        let DirectoryTreeReply::Prepared {
            staging_device_root,
            ..
        } = service
            .execute(
                DirectoryTreeRequest::PreparePut {
                    manifest: manifest.clone(),
                },
                true,
                true,
            )
            .await
        else {
            panic!("expected staging before tamper");
        };
        fs::write(PathBuf::from(staging_device_root).join("a.txt"), b"b").unwrap();
        assert!(matches!(
            service
                .execute(DirectoryTreeRequest::FinalizePut { manifest }, true, true)
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::HashMismatch
        ));
    }

    #[tokio::test]
    async fn host_directory_get_is_read_authorized_and_reproves_source_tree() {
        let temp = tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        let source = allowed.join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("a.txt"), b"alpha").unwrap();
        fs::write(source.join("nested/b.txt"), b"beta").unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let policy =
            ReadPolicy::new(vec![allowed.to_string_lossy().into_owned()], 49_152, 5_000).unwrap();
        let service =
            HostDirectoryTreeService::new(policy, controller_id, site_id, device_id, true, false)
                .unwrap();
        let transfer_id = TransferId::new();
        let scope = DirectoryTreeGetScope::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            source.to_string_lossy(),
        )
        .unwrap();

        let DirectoryTreeReply::Manifest { manifest } = service
            .execute(
                DirectoryTreeRequest::PrepareGet {
                    scope: scope.clone(),
                },
                true,
                false,
            )
            .await
        else {
            panic!("expected device directory manifest");
        };
        assert_eq!(manifest.transfer_id, transfer_id);
        assert_eq!(
            manifest.direction,
            FileTransferDirection::DeviceToController
        );
        assert_eq!(manifest.device_conflict_policy, None);
        assert_eq!(manifest.total_file_bytes, 9);
        assert!(manifest.entries.iter().any(|entry| {
            entry.relative_path == "empty" && entry.kind == DirectoryTreeEntryKind::Directory
        }));

        let put_manifest = DirectoryTreeManifest::new(
            TransferId::new(),
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            allowed.join("put-denied").to_string_lossy(),
            vec![],
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();
        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::PreparePut {
                        manifest: put_manifest,
                    },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::Denied
        ));

        fs::write(source.join("a.txt"), b"changed").unwrap();
        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::FinalizeGet {
                        manifest: manifest.clone(),
                    },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::HashMismatch
        ));
        fs::write(source.join("a.txt"), b"alpha").unwrap();
        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::FinalizeGet { manifest },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Verified {
                transfer_id: actual,
                ..
            } if actual == transfer_id
        ));

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let outside_scope = DirectoryTreeGetScope::new(
            TransferId::new(),
            controller_id,
            site_id,
            device_id,
            outside.to_string_lossy(),
        )
        .unwrap();
        assert!(matches!(
            service
                .execute(
                    DirectoryTreeRequest::PrepareGet {
                        scope: outside_scope,
                    },
                    true,
                    false,
                )
                .await,
            DirectoryTreeReply::Error(error) if error.code == DirectoryTreeErrorCode::Denied
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
