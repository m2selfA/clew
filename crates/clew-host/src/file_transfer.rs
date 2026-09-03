use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Seek as _, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use clew_core::{ControllerId, DeviceId, ReadPolicy, SiteId, TransferId};
use clew_transport::{
    FileConflictPolicy, FileResumeDescriptor, FileTransferChunk, FileTransferDirection,
    FileTransferErrorCode, FileTransferManifest, FileTransferPhase, FileTransferReply,
    FileTransferRequest, FileTransferStatus,
};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

pub const HARD_MAX_HOST_FILE_TRANSFERS: usize = 16;
const HARD_MAX_RENAME_ATTEMPTS: u32 = 1024;

#[derive(Clone, Debug)]
pub struct HostFileTransferService {
    policy: ReadPolicy,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    inner: Arc<Mutex<HostFileTransferStore>>,
}

#[derive(Debug, Default)]
struct HostFileTransferStore {
    next_sequence: u64,
    entries: BTreeMap<TransferId, HostPutEntry>,
}

#[derive(Debug)]
struct HostPutEntry {
    sequence: u64,
    manifest: FileTransferManifest,
    descriptor: FileResumeDescriptor,
    phase: FileTransferPhase,
    requested_target: PathBuf,
    temp: Option<NamedTempFile>,
    prefix_hasher: Sha256,
    last_chunk: Option<LastChunk>,
    final_device_path: Option<String>,
}

#[derive(Clone, Debug)]
struct LastChunk {
    offset: u64,
    len: usize,
    sha256: String,
}

impl HostPutEntry {
    fn status(&self) -> FileTransferReply {
        FileTransferReply::Status(FileTransferStatus {
            descriptor: self.descriptor.clone(),
            phase: self.phase,
            final_device_path: self.final_device_path.clone(),
        })
    }
}

impl HostFileTransferService {
    pub fn new(
        policy: ReadPolicy,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self {
            policy,
            controller_id,
            site_id,
            device_id,
            inner: Arc::new(Mutex::new(HostFileTransferStore::default())),
        })
    }

    pub async fn execute(
        &self,
        request: FileTransferRequest,
        allow_write: bool,
    ) -> FileTransferReply {
        if request.validate().is_err() {
            return FileTransferReply::error(
                FileTransferErrorCode::InvalidRequest,
                "invalid bounded file transfer request",
            );
        }
        if !allow_write || self.policy.roots.is_empty() {
            return FileTransferReply::error(
                FileTransferErrorCode::Denied,
                "file put is outside the allowed device grant",
            );
        }
        let service = self.clone();
        match tokio::task::spawn_blocking(move || service.execute_blocking(request)).await {
            Ok(reply) => reply,
            Err(_) => {
                FileTransferReply::error(FileTransferErrorCode::Io, "file transfer worker failed")
            }
        }
    }

    fn execute_blocking(&self, request: FileTransferRequest) -> FileTransferReply {
        match request {
            FileTransferRequest::PutBegin { manifest } => self.put_begin(manifest),
            FileTransferRequest::PutChunk { chunk } => self.put_chunk(chunk),
            FileTransferRequest::Status { transfer_id } => self.status(transfer_id),
            FileTransferRequest::Finalize { transfer_id } => self.finalize(transfer_id),
            FileTransferRequest::Cancel { transfer_id } => self.cancel(transfer_id),
        }
    }

    fn put_begin(&self, manifest: FileTransferManifest) -> FileTransferReply {
        if manifest.controller_id != self.controller_id
            || manifest.site_id != self.site_id
            || manifest.device_id != self.device_id
            || manifest.direction != FileTransferDirection::ControllerToDevice
        {
            return transfer_denied(
                "file transfer manifest scope does not match this Host membership",
            );
        }
        {
            let store = match self.inner.lock() {
                Ok(store) => store,
                Err(_) => return transfer_io("Host transfer store is unavailable"),
            };
            if let Some(existing) = store.entries.get(&manifest.transfer_id) {
                return if existing.manifest == manifest {
                    existing.status()
                } else {
                    FileTransferReply::error(
                        FileTransferErrorCode::Conflict,
                        "TransferId is already bound to a different manifest",
                    )
                };
            }
        }

        let requested_target = match prepare_put_target(&self.policy, &manifest) {
            Ok(target) => target,
            Err(reply) => return reply,
        };
        let Some(parent) = requested_target.parent() else {
            return FileTransferReply::error(
                FileTransferErrorCode::InvalidRequest,
                "device target parent is invalid",
            );
        };
        let temp = match NamedTempFile::new_in(parent) {
            Ok(temp) => temp,
            Err(_) => return transfer_io("secure transfer part file could not be created"),
        };
        let descriptor = match manifest.initial_resume_descriptor() {
            Ok(descriptor) => descriptor,
            Err(_) => {
                return FileTransferReply::error(
                    FileTransferErrorCode::InvalidRequest,
                    "initial resume descriptor is invalid",
                );
            }
        };
        let phase = if manifest.total_size == 0 {
            FileTransferPhase::ReadyToFinalize
        } else {
            FileTransferPhase::Receiving
        };
        let transfer_id = manifest.transfer_id;
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        prune_completed(&mut store);
        if let Some(existing) = store.entries.get(&transfer_id) {
            return if existing.manifest == manifest {
                existing.status()
            } else {
                FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "TransferId is already bound to a different manifest",
                )
            };
        }
        if store.entries.len() >= HARD_MAX_HOST_FILE_TRANSFERS {
            return FileTransferReply::error(
                FileTransferErrorCode::Capacity,
                "Host file transfer capacity is exhausted",
            );
        }
        store.next_sequence = store.next_sequence.saturating_add(1);
        let sequence = store.next_sequence;
        store.entries.insert(
            transfer_id,
            HostPutEntry {
                sequence,
                manifest,
                descriptor,
                phase,
                requested_target,
                temp: Some(temp),
                prefix_hasher: Sha256::new(),
                last_chunk: None,
                final_device_path: None,
            },
        );
        store
            .entries
            .get(&transfer_id)
            .map(HostPutEntry::status)
            .unwrap_or_else(|| transfer_io("Host transfer state failed"))
    }

    fn put_chunk(&self, chunk: FileTransferChunk) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        let Some(entry) = store.entries.get_mut(&chunk.transfer_id) else {
            return transfer_not_found();
        };
        let bytes = match chunk.decode_bytes() {
            Ok(bytes) => bytes,
            Err(_) => {
                return FileTransferReply::error(
                    FileTransferErrorCode::HashMismatch,
                    "file chunk failed its SHA-256 verification",
                );
            }
        };
        if let Some(last) = &entry.last_chunk
            && chunk.offset == last.offset
            && chunk.sha256 == last.sha256
            && bytes.len() == last.len
            && chunk.offset.saturating_add(bytes.len() as u64) == entry.descriptor.confirmed_offset
        {
            return entry.status();
        }
        if entry.phase == FileTransferPhase::Completed {
            return entry.status();
        }
        if entry.phase == FileTransferPhase::ReadyToFinalize {
            return FileTransferReply::error(
                FileTransferErrorCode::OutOfOrder,
                "all chunks are already confirmed; finalize is required",
            );
        }
        if chunk.offset != entry.descriptor.confirmed_offset {
            return FileTransferReply::error(
                FileTransferErrorCode::OutOfOrder,
                "file chunk does not start at the confirmed contiguous offset",
            );
        }
        if entry.manifest.validate_chunk(&chunk).is_err() {
            return FileTransferReply::error(
                FileTransferErrorCode::InvalidRequest,
                "file chunk does not match the transfer manifest",
            );
        }
        let next_offset = entry
            .descriptor
            .confirmed_offset
            .saturating_add(bytes.len() as u64);
        let mut next_hasher = entry.prefix_hasher.clone();
        next_hasher.update(&bytes);
        let next_prefix = digest_hex(next_hasher.clone().finalize());
        if next_offset == entry.manifest.total_size && next_prefix != entry.manifest.final_sha256 {
            return FileTransferReply::error(
                FileTransferErrorCode::HashMismatch,
                "complete file does not match the expected final SHA-256",
            );
        }
        let Some(temp) = entry.temp.as_mut() else {
            return transfer_io("file transfer part file is unavailable");
        };
        let file = temp.as_file_mut();
        match file.metadata() {
            Ok(metadata) if metadata.len() == entry.descriptor.confirmed_offset => {}
            _ => return transfer_io("file transfer part length disagrees with checkpoint"),
        }
        if file.seek(SeekFrom::Start(chunk.offset)).is_err()
            || file.write_all(&bytes).is_err()
            || file.sync_data().is_err()
        {
            return transfer_io("file chunk could not be durably written");
        }
        entry.prefix_hasher = next_hasher;
        entry.descriptor.checkpoint_revision =
            entry.descriptor.checkpoint_revision.saturating_add(1);
        entry.descriptor.confirmed_offset = next_offset;
        entry.descriptor.confirmed_prefix_sha256 = next_prefix;
        entry.last_chunk = Some(LastChunk {
            offset: chunk.offset,
            len: bytes.len(),
            sha256: chunk.sha256,
        });
        entry.phase = if next_offset == entry.manifest.total_size {
            FileTransferPhase::ReadyToFinalize
        } else {
            FileTransferPhase::Receiving
        };
        if entry.descriptor.validate().is_err() {
            return transfer_io("Host generated an invalid resume checkpoint");
        }
        entry.status()
    }

    fn status(&self, transfer_id: TransferId) -> FileTransferReply {
        let store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        store
            .entries
            .get(&transfer_id)
            .map(HostPutEntry::status)
            .unwrap_or_else(transfer_not_found)
    }

    fn finalize(&self, transfer_id: TransferId) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        let Some(entry) = store.entries.get_mut(&transfer_id) else {
            return transfer_not_found();
        };
        if entry.phase == FileTransferPhase::Completed {
            return entry.status();
        }
        if entry.phase != FileTransferPhase::ReadyToFinalize {
            return FileTransferReply::error(
                FileTransferErrorCode::OutOfOrder,
                "file transfer has not confirmed all chunks",
            );
        }
        if entry.descriptor.confirmed_prefix_sha256 != entry.manifest.final_sha256 {
            return FileTransferReply::error(
                FileTransferErrorCode::HashMismatch,
                "confirmed file prefix does not match the expected final SHA-256",
            );
        }
        let Some(temp) = entry.temp.take() else {
            return transfer_io("file transfer part file is unavailable");
        };
        match persist_final(
            temp,
            &entry.requested_target,
            entry.manifest.device_conflict_policy,
        ) {
            Ok(finalized) => {
                entry.phase = FileTransferPhase::Completed;
                entry.final_device_path = finalized.to_str().map(str::to_owned);
                if entry.final_device_path.is_none() {
                    return transfer_io("finalized device path is not valid UTF-8");
                }
                entry.status()
            }
            Err(PersistFinalError::BeforeCommit { reply, temp }) => {
                entry.temp = Some(temp);
                reply
            }
            Err(PersistFinalError::AfterCommit { reply, path }) => {
                entry.phase = FileTransferPhase::Completed;
                entry.final_device_path = path.to_str().map(str::to_owned);
                reply
            }
        }
    }

    fn cancel(&self, transfer_id: TransferId) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        let Some(entry) = store.entries.get(&transfer_id) else {
            return transfer_not_found();
        };
        if entry.phase == FileTransferPhase::Completed {
            return FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "completed file transfer cannot be cancelled",
            );
        }
        store.entries.remove(&transfer_id);
        FileTransferReply::Cancelled { transfer_id }
    }
}

fn prune_completed(store: &mut HostFileTransferStore) {
    while store.entries.len() >= HARD_MAX_HOST_FILE_TRANSFERS {
        let candidate = store
            .entries
            .iter()
            .filter_map(|(transfer_id, entry)| {
                (entry.phase == FileTransferPhase::Completed)
                    .then_some((*transfer_id, entry.sequence))
            })
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(transfer_id, _)| transfer_id);
        let Some(transfer_id) = candidate else {
            break;
        };
        store.entries.remove(&transfer_id);
    }
}

fn prepare_put_target(
    policy: &ReadPolicy,
    manifest: &FileTransferManifest,
) -> Result<PathBuf, FileTransferReply> {
    let requested = PathBuf::from(&manifest.device_path);
    if !requested.is_absolute() {
        return Err(transfer_denied("device destination must be absolute"));
    }
    let Some(Component::Normal(file_name)) = requested.components().next_back() else {
        return Err(FileTransferReply::error(
            FileTransferErrorCode::InvalidRequest,
            "device destination must end in a normal filename",
        ));
    };
    let Some(parent) = requested.parent() else {
        return Err(FileTransferReply::error(
            FileTransferErrorCode::InvalidRequest,
            "device destination parent is invalid",
        ));
    };
    let parent = fs::canonicalize(parent).map_err(|_| {
        FileTransferReply::error(
            FileTransferErrorCode::NotFound,
            "device destination parent was not found",
        )
    })?;
    ensure_allowed_parent(policy, &parent)?;
    let target = parent.join(file_name);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(transfer_denied("device destination cannot be a symlink"));
        }
        Ok(metadata) if metadata.is_dir() => {
            return Err(FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "device destination is a directory",
            ));
        }
        Ok(_) if manifest.device_conflict_policy == Some(FileConflictPolicy::FailIfExists) => {
            return Err(FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "device destination already exists",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(transfer_io("device destination metadata failed")),
    }
    Ok(target)
}

fn ensure_allowed_parent(policy: &ReadPolicy, parent: &Path) -> Result<(), FileTransferReply> {
    for root in &policy.roots {
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if parent.starts_with(root) {
            return Ok(());
        }
    }
    Err(transfer_denied(
        "device destination is outside signed roots",
    ))
}

enum PersistFinalError {
    BeforeCommit {
        reply: FileTransferReply,
        temp: NamedTempFile,
    },
    AfterCommit {
        reply: FileTransferReply,
        path: PathBuf,
    },
}

fn persist_final(
    temp: NamedTempFile,
    requested_target: &Path,
    conflict: Option<FileConflictPolicy>,
) -> Result<PathBuf, PersistFinalError> {
    match conflict {
        Some(FileConflictPolicy::FailIfExists) => persist_noclobber(temp, requested_target),
        Some(FileConflictPolicy::ReplaceExisting) => {
            if let Ok(metadata) = fs::symlink_metadata(requested_target)
                && (metadata.file_type().is_symlink() || metadata.is_dir())
            {
                return Err(PersistFinalError::BeforeCommit {
                    reply: transfer_denied("device destination cannot be replaced safely"),
                    temp,
                });
            }
            match temp.persist(requested_target) {
                Ok(file) => match finalize_persisted(file, requested_target) {
                    Ok(()) => Ok(requested_target.to_path_buf()),
                    Err(reply) => Err(PersistFinalError::AfterCommit {
                        reply,
                        path: requested_target.to_path_buf(),
                    }),
                },
                Err(error) => Err(PersistFinalError::BeforeCommit {
                    reply: transfer_io("atomic replace failed"),
                    temp: error.file,
                }),
            }
        }
        Some(FileConflictPolicy::RenameIfExists) => persist_with_rename(temp, requested_target),
        None => Err(PersistFinalError::BeforeCommit {
            reply: FileTransferReply::error(
                FileTransferErrorCode::InvalidRequest,
                "device conflict policy is missing",
            ),
            temp,
        }),
    }
}

fn persist_noclobber(temp: NamedTempFile, target: &Path) -> Result<PathBuf, PersistFinalError> {
    match temp.persist_noclobber(target) {
        Ok(file) => match finalize_persisted(file, target) {
            Ok(()) => Ok(target.to_path_buf()),
            Err(reply) => Err(PersistFinalError::AfterCommit {
                reply,
                path: target.to_path_buf(),
            }),
        },
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(PersistFinalError::BeforeCommit {
                reply: FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "device destination already exists",
                ),
                temp: error.file,
            })
        }
        Err(error) => Err(PersistFinalError::BeforeCommit {
            reply: transfer_io("atomic create failed"),
            temp: error.file,
        }),
    }
}

fn persist_with_rename(
    mut temp: NamedTempFile,
    requested_target: &Path,
) -> Result<PathBuf, PersistFinalError> {
    let mut candidate = requested_target.to_path_buf();
    for attempt in 0..=HARD_MAX_RENAME_ATTEMPTS {
        match temp.persist_noclobber(&candidate) {
            Ok(file) => {
                return match finalize_persisted(file, &candidate) {
                    Ok(()) => Ok(candidate),
                    Err(reply) => Err(PersistFinalError::AfterCommit {
                        reply,
                        path: candidate,
                    }),
                };
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temp = error.file;
                let Some(next) = rename_candidate(requested_target, attempt.saturating_add(1))
                else {
                    return Err(PersistFinalError::BeforeCommit {
                        reply: FileTransferReply::error(
                            FileTransferErrorCode::Conflict,
                            "no bounded rename candidate is available",
                        ),
                        temp,
                    });
                };
                candidate = next;
            }
            Err(error) => {
                return Err(PersistFinalError::BeforeCommit {
                    reply: transfer_io("atomic renamed persist failed"),
                    temp: error.file,
                });
            }
        }
    }
    Err(PersistFinalError::BeforeCommit {
        reply: FileTransferReply::error(
            FileTransferErrorCode::Conflict,
            "rename conflict attempts exhausted",
        ),
        temp,
    })
}

fn rename_candidate(path: &Path, attempt: u32) -> Option<PathBuf> {
    let parent = path.parent()?;
    let file_name = path.file_name()?.to_str()?;
    let candidate = parent.join(format!("{file_name}.clew-{attempt}"));
    (candidate.to_string_lossy().len() <= clew_core::HARD_MAX_READ_ROOT_BYTES).then_some(candidate)
}

fn finalize_persisted(file: File, target: &Path) -> Result<(), FileTransferReply> {
    let _ = target;
    file.sync_all()
        .map_err(|_| transfer_io("finalized device file sync failed"))?;
    #[cfg(unix)]
    if let Some(parent) = target.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| transfer_io("device destination directory sync failed"))?;
    }
    Ok(())
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn transfer_denied(message: &'static str) -> FileTransferReply {
    FileTransferReply::error(FileTransferErrorCode::Denied, message)
}

fn transfer_io(message: &'static str) -> FileTransferReply {
    FileTransferReply::error(FileTransferErrorCode::Io, message)
}

fn transfer_not_found() -> FileTransferReply {
    FileTransferReply::error(
        FileTransferErrorCode::NotFound,
        "file transfer was not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_transport::{FileTransferRequest, file_sha256_hex};
    use tempfile::tempdir;

    fn manifest(
        root: &Path,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        transfer_id: TransferId,
        bytes: &[u8],
        conflict: FileConflictPolicy,
    ) -> FileTransferManifest {
        FileTransferManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            root.join("target.bin").to_string_lossy(),
            bytes.len() as u64,
            4096,
            file_sha256_hex(bytes),
            Some(conflict),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn put_chunks_resume_finalize_and_cancel_are_bounded_and_idempotent() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let service = HostFileTransferService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 5_000).unwrap(),
            controller_id,
            site_id,
            device_id,
        )
        .unwrap();
        let bytes = vec![0x5a; 5_000];
        let transfer_id = TransferId::new();
        let put_manifest = manifest(
            &root,
            controller_id,
            site_id,
            device_id,
            transfer_id,
            &bytes,
            FileConflictPolicy::FailIfExists,
        );
        let FileTransferReply::Status(begin) = service
            .execute(
                FileTransferRequest::PutBegin {
                    manifest: put_manifest.clone(),
                },
                true,
            )
            .await
        else {
            panic!("expected put begin status");
        };
        assert_eq!(begin.phase, FileTransferPhase::Receiving);
        assert_eq!(begin.descriptor.confirmed_offset, 0);
        assert!(!root.join("target.bin").exists());

        let first = FileTransferChunk::from_bytes(transfer_id, 0, &bytes[..4096]).unwrap();
        let FileTransferReply::Status(after_first) = service
            .execute(
                FileTransferRequest::PutChunk {
                    chunk: first.clone(),
                },
                true,
            )
            .await
        else {
            panic!("expected first chunk status");
        };
        assert_eq!(after_first.descriptor.confirmed_offset, 4096);
        assert_eq!(after_first.phase, FileTransferPhase::Receiving);
        let replay = service
            .execute(FileTransferRequest::PutChunk { chunk: first }, true)
            .await;
        assert_eq!(replay, FileTransferReply::Status(after_first.clone()));

        let status = service
            .execute(FileTransferRequest::Status { transfer_id }, true)
            .await;
        assert_eq!(status, FileTransferReply::Status(after_first));

        let last = FileTransferChunk::from_bytes(transfer_id, 4096, &bytes[4096..]).unwrap();
        let FileTransferReply::Status(ready) = service
            .execute(FileTransferRequest::PutChunk { chunk: last }, true)
            .await
        else {
            panic!("expected ready-to-finalize status");
        };
        assert_eq!(ready.phase, FileTransferPhase::ReadyToFinalize);
        assert_eq!(ready.descriptor.confirmed_offset, bytes.len() as u64);
        assert_eq!(
            ready.descriptor.confirmed_prefix_sha256,
            file_sha256_hex(&bytes)
        );
        assert!(!root.join("target.bin").exists());

        let FileTransferReply::Status(completed) = service
            .execute(FileTransferRequest::Finalize { transfer_id }, true)
            .await
        else {
            panic!("expected completed transfer status");
        };
        assert_eq!(completed.phase, FileTransferPhase::Completed);
        assert_eq!(fs::read(root.join("target.bin")).unwrap(), bytes);
        assert_eq!(
            service
                .execute(FileTransferRequest::Finalize { transfer_id }, true)
                .await,
            FileTransferReply::Status(completed)
        );

        let cancel_id = TransferId::new();
        let cancel_manifest = manifest(
            &root,
            controller_id,
            site_id,
            device_id,
            cancel_id,
            b"cancel",
            FileConflictPolicy::RenameIfExists,
        );
        service
            .execute(
                FileTransferRequest::PutBegin {
                    manifest: cancel_manifest,
                },
                true,
            )
            .await;
        assert_eq!(
            service
                .execute(
                    FileTransferRequest::Cancel {
                        transfer_id: cancel_id,
                    },
                    true,
                )
                .await,
            FileTransferReply::Cancelled {
                transfer_id: cancel_id
            }
        );
        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::Status {
                        transfer_id: cancel_id,
                    },
                    true,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::NotFound
        ));
    }

    #[tokio::test]
    async fn put_scope_authority_conflict_and_hash_errors_fail_closed() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let service = HostFileTransferService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 5_000).unwrap(),
            controller_id,
            site_id,
            device_id,
        )
        .unwrap();
        let transfer_id = TransferId::new();
        let good = manifest(
            &root,
            controller_id,
            site_id,
            device_id,
            transfer_id,
            b"hello",
            FileConflictPolicy::FailIfExists,
        );
        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::PutBegin {
                        manifest: good.clone(),
                    },
                    false,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));
        let mut wrong_scope = good.clone();
        wrong_scope.device_id = DeviceId::new();
        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::PutBegin {
                        manifest: wrong_scope,
                    },
                    true,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));

        fs::write(root.join("target.bin"), b"existing").unwrap();
        assert!(matches!(
            service
                .execute(FileTransferRequest::PutBegin { manifest: good }, true)
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Conflict
        ));
    }
}
