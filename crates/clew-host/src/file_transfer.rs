use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use clew_core::{
    ControllerId, DeviceId, MAX_STATE_DOCUMENT_SIZE, ReadPolicy, SiteId, StateCodecError,
    StateLayout, TransferId, decode_state_json, encode_state_json,
};
use clew_transport::{
    FileConflictPolicy, FileResumeDescriptor, FileTransferChunk, FileTransferDirection,
    FileTransferErrorCode, FileTransferManifest, FileTransferPhase, FileTransferReply,
    FileTransferRequest, FileTransferStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::target_path::expand_target_path;

pub const HARD_MAX_HOST_FILE_TRANSFERS: usize = 16;
const HARD_MAX_RENAME_ATTEMPTS: u32 = 1024;
const HOST_TRANSFER_STATE_FILE_A: &str = "state.a.json";
const HOST_TRANSFER_STATE_FILE_B: &str = "state.b.json";

#[derive(Clone, Debug)]
pub struct HostFileTransferService {
    policy: ReadPolicy,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    can_get: bool,
    can_put: bool,
    inner: Arc<Mutex<HostFileTransferStore>>,
    state_store: Option<Arc<HostFileTransferStateStore>>,
}

#[derive(Debug, Default)]
struct HostFileTransferStore {
    next_sequence: u64,
    puts: BTreeMap<TransferId, HostPutEntry>,
    gets: BTreeMap<TransferId, HostGetEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct HostFileTransferSnapshot {
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    generation: u64,
    puts: Vec<DurableHostPut>,
    gets: Vec<DurableHostGet>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableHostPut {
    sequence: u64,
    manifest: FileTransferManifest,
    descriptor: FileResumeDescriptor,
    phase: FileTransferPhase,
    requested_target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    part_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_device_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finalizing_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableHostGet {
    requested_device_path: String,
    chunk_size: u32,
    manifest: FileTransferManifest,
}

#[derive(Debug)]
struct HostFileTransferStateStore {
    root: PathBuf,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    generation: Mutex<u64>,
}

impl HostFileTransferSnapshot {
    fn validate(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> Result<(), HostFileTransferStateError> {
        if self.controller_id != controller_id
            || self.site_id != site_id
            || self.device_id != device_id
        {
            return Err(HostFileTransferStateError::ScopeMismatch);
        }
        if self.puts.len().saturating_add(self.gets.len()) > HARD_MAX_HOST_FILE_TRANSFERS {
            return Err(HostFileTransferStateError::InvalidState(
                "Host file transfer journal exceeds the hard entry bound".into(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for put in &self.puts {
            put.manifest.validate().map_err(|error| {
                HostFileTransferStateError::InvalidState(format!(
                    "invalid persisted Put manifest: {error}"
                ))
            })?;
            put.descriptor.validate().map_err(|error| {
                HostFileTransferStateError::InvalidState(format!(
                    "invalid persisted Put checkpoint: {error}"
                ))
            })?;
            if put.manifest.transfer_id != put.descriptor.transfer_id
                || put.manifest.controller_id != controller_id
                || put.manifest.site_id != site_id
                || put.manifest.device_id != device_id
                || put.manifest.direction != FileTransferDirection::ControllerToDevice
                || put.descriptor.device_path != put.manifest.device_path
                || put.descriptor.total_size != put.manifest.total_size
                || put.descriptor.final_sha256.as_ref() != Some(&put.manifest.final_sha256)
                || !ids.insert(put.manifest.transfer_id)
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "persisted Put scope/checkpoint is inconsistent".into(),
                ));
            }
            if put.requested_target.trim().is_empty()
                || put.requested_target.len() > clew_core::HARD_MAX_READ_ROOT_BYTES
                || !Path::new(&put.requested_target).is_absolute()
                || put.part_path.as_ref().is_some_and(|path| {
                    path.trim().is_empty()
                        || path.len() > clew_core::HARD_MAX_READ_ROOT_BYTES
                        || !Path::new(path).is_absolute()
                })
                || put.finalizing_path.as_ref().is_some_and(|path| {
                    path.trim().is_empty()
                        || path.len() > clew_core::HARD_MAX_READ_ROOT_BYTES
                        || !Path::new(path).is_absolute()
                        || !is_valid_finalizing_path(
                            Path::new(&put.requested_target),
                            Path::new(path),
                            put.manifest.device_conflict_policy,
                        )
                })
                || put.finalizing_path.is_some()
                    && (put.phase != FileTransferPhase::ReadyToFinalize
                        || put.descriptor.confirmed_offset != put.manifest.total_size)
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "persisted Put private path is invalid".into(),
                ));
            }
        }
        for get in &self.gets {
            get.manifest.validate().map_err(|error| {
                HostFileTransferStateError::InvalidState(format!(
                    "invalid persisted Get manifest: {error}"
                ))
            })?;
            if get.manifest.controller_id != controller_id
                || get.manifest.site_id != site_id
                || get.manifest.device_id != device_id
                || get.manifest.direction != FileTransferDirection::DeviceToController
                || get.chunk_size != get.manifest.chunk_size
                || get.requested_device_path.trim().is_empty()
                || !ids.insert(get.manifest.transfer_id)
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "persisted Get binding is inconsistent".into(),
                ));
            }
        }
        Ok(())
    }
}

impl HostFileTransferStateStore {
    fn load(
        layout: &StateLayout,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> Result<(Arc<Self>, HostFileTransferSnapshot), HostFileTransferStateError> {
        let root = layout.host_file_transfers_root(controller_id, site_id);
        let mut valid = Vec::new();
        let mut first_error = None;
        let mut any_present = false;
        for path in [
            root.join(HOST_TRANSFER_STATE_FILE_A),
            root.join(HOST_TRANSFER_STATE_FILE_B),
        ] {
            match read_host_transfer_slot(&path) {
                HostTransferSlotRead::Missing => {}
                HostTransferSlotRead::Valid(snapshot) => {
                    any_present = true;
                    match snapshot.validate(controller_id, site_id, device_id) {
                        Ok(()) => valid.push(snapshot),
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                HostTransferSlotRead::Invalid(error) => {
                    any_present = true;
                    first_error.get_or_insert(error);
                }
            }
        }
        if !valid.is_empty() {
            valid.sort_by_key(|snapshot| snapshot.generation);
            if valid.len() == 2 && valid[0].generation == valid[1].generation {
                return Err(HostFileTransferStateError::GenerationConflict(
                    valid[0].generation,
                ));
            }
            let snapshot = valid.pop().expect("valid Host transfer snapshot exists");
            let store = Arc::new(Self {
                root,
                controller_id,
                site_id,
                device_id,
                generation: Mutex::new(snapshot.generation),
            });
            return Ok((store, snapshot));
        }
        if any_present {
            return Err(first_error.unwrap_or_else(|| {
                HostFileTransferStateError::InvalidState(
                    "Host file transfer journal has no valid slot".into(),
                )
            }));
        }
        let snapshot = HostFileTransferSnapshot {
            controller_id,
            site_id,
            device_id,
            generation: 0,
            puts: Vec::new(),
            gets: Vec::new(),
        };
        let store = Arc::new(Self {
            root,
            controller_id,
            site_id,
            device_id,
            generation: Mutex::new(0),
        });
        Ok((store, snapshot))
    }

    fn persist(&self, state: &HostFileTransferStore) -> Result<(), HostFileTransferStateError> {
        let mut generation = self
            .generation
            .lock()
            .map_err(|_| HostFileTransferStateError::StatePoisoned)?;
        let next_generation = generation
            .checked_add(1)
            .ok_or(HostFileTransferStateError::GenerationOverflow)?;
        let snapshot = HostFileTransferSnapshot {
            controller_id: self.controller_id,
            site_id: self.site_id,
            device_id: self.device_id,
            generation: next_generation,
            puts: state
                .puts
                .values()
                .map(|entry| DurableHostPut {
                    sequence: entry.sequence,
                    manifest: entry.manifest.clone(),
                    descriptor: entry.descriptor.clone(),
                    phase: entry.phase,
                    requested_target: entry.requested_target.to_string_lossy().into_owned(),
                    part_path: entry
                        .temp
                        .as_ref()
                        .map(|temp| temp.path().to_string_lossy().into_owned()),
                    final_device_path: entry.final_device_path.clone(),
                    finalizing_path: entry
                        .finalizing_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                })
                .collect(),
            gets: state
                .gets
                .values()
                .map(|entry| DurableHostGet {
                    requested_device_path: entry.requested_device_path.clone(),
                    chunk_size: entry.chunk_size,
                    manifest: entry.manifest.clone(),
                })
                .collect(),
        };
        snapshot.validate(self.controller_id, self.site_id, self.device_id)?;
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        let path = if next_generation % 2 == 0 {
            self.root.join(HOST_TRANSFER_STATE_FILE_A)
        } else {
            self.root.join(HOST_TRANSFER_STATE_FILE_B)
        };
        write_host_transfer_slot(&path, &snapshot)?;
        *generation = next_generation;
        Ok(())
    }
}

enum HostTransferSlotRead {
    Missing,
    Valid(HostFileTransferSnapshot),
    Invalid(HostFileTransferStateError),
}

fn read_host_transfer_slot(path: &Path) -> HostTransferSlotRead {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return HostTransferSlotRead::Missing;
        }
        Err(error) => return HostTransferSlotRead::Invalid(error.into()),
    };
    let mut encoded = Vec::new();
    if let Err(error) = std::io::Read::by_ref(&mut file)
        .take((MAX_STATE_DOCUMENT_SIZE + 1) as u64)
        .read_to_end(&mut encoded)
    {
        return HostTransferSlotRead::Invalid(error.into());
    }
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return HostTransferSlotRead::Invalid(HostFileTransferStateError::InvalidState(
            "Host transfer journal exceeds the state document hard bound".into(),
        ));
    }
    match decode_state_json(&encoded) {
        Ok(snapshot) => HostTransferSlotRead::Valid(snapshot),
        Err(error) => HostTransferSlotRead::Invalid(error.into()),
    }
}

fn write_host_transfer_slot(
    path: &Path,
    snapshot: &HostFileTransferSnapshot,
) -> Result<(), HostFileTransferStateError> {
    let encoded = encode_state_json(snapshot)?;
    let parent = path
        .parent()
        .ok_or_else(|| HostFileTransferStateError::InvalidState("invalid state path".into()))?;
    fs::create_dir_all(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum HostFileTransferStateError {
    #[error("Host file transfer state belongs to another Controller/Site/Device")]
    ScopeMismatch,
    #[error("Host file transfer persisted state is invalid: {0}")]
    InvalidState(String),
    #[error("Host file transfer journal has conflicting generation {0}")]
    GenerationConflict(u64),
    #[error("Host file transfer journal generation overflow")]
    GenerationOverflow,
    #[error("Host file transfer state is poisoned")]
    StatePoisoned,
    #[error(transparent)]
    StateCodec(#[from] StateCodecError),
    #[error("Host file transfer state I/O failed: {0}")]
    Io(#[from] std::io::Error),
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
    finalizing_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct LastChunk {
    offset: u64,
    len: usize,
    sha256: String,
}

#[derive(Debug)]
struct HostGetEntry {
    requested_device_path: String,
    chunk_size: u32,
    manifest: FileTransferManifest,
    file: File,
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

impl Drop for HostFileTransferService {
    fn drop(&mut self) {
        if self.state_store.is_none() || Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let Ok(mut store) = self.inner.lock() else {
            return;
        };
        for entry in store.puts.values_mut() {
            if entry.phase != FileTransferPhase::Completed
                && let Some(temp) = entry.temp.take()
            {
                let _ = temp.keep();
            }
        }
    }
}

impl HostFileTransferService {
    pub fn new(
        policy: ReadPolicy,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        can_get: bool,
        can_put: bool,
    ) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self {
            policy,
            controller_id,
            site_id,
            device_id,
            can_get,
            can_put,
            inner: Arc::new(Mutex::new(HostFileTransferStore::default())),
            state_store: None,
        })
    }

    pub fn load_or_create(
        policy: ReadPolicy,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        can_get: bool,
        can_put: bool,
        layout: &StateLayout,
    ) -> Result<Self, HostFileTransferStateError> {
        policy.validate().map_err(|error| {
            HostFileTransferStateError::InvalidState(format!("invalid signed file policy: {error}"))
        })?;
        let (state_store, snapshot) =
            HostFileTransferStateStore::load(layout, controller_id, site_id, device_id)?;
        let service = Self {
            policy,
            controller_id,
            site_id,
            device_id,
            can_get,
            can_put,
            inner: Arc::new(Mutex::new(HostFileTransferStore::default())),
            state_store: Some(state_store),
        };
        service.restore_snapshot(snapshot)?;
        Ok(service)
    }

    fn restore_snapshot(
        &self,
        snapshot: HostFileTransferSnapshot,
    ) -> Result<(), HostFileTransferStateError> {
        snapshot.validate(self.controller_id, self.site_id, self.device_id)?;
        let mut store = self
            .inner
            .lock()
            .map_err(|_| HostFileTransferStateError::StatePoisoned)?;
        store.next_sequence = snapshot
            .puts
            .iter()
            .map(|put| put.sequence)
            .max()
            .unwrap_or(0);
        for durable in snapshot.puts {
            if durable.phase == FileTransferPhase::Completed {
                store.puts.insert(
                    durable.manifest.transfer_id,
                    HostPutEntry {
                        sequence: durable.sequence,
                        manifest: durable.manifest,
                        descriptor: durable.descriptor,
                        phase: FileTransferPhase::Completed,
                        requested_target: PathBuf::from(durable.requested_target),
                        temp: None,
                        prefix_hasher: Sha256::new(),
                        last_chunk: None,
                        final_device_path: durable.final_device_path,
                        finalizing_path: durable.finalizing_path.map(PathBuf::from),
                    },
                );
                continue;
            }
            let part_path = durable.part_path.ok_or_else(|| {
                HostFileTransferStateError::InvalidState(
                    "active Host Put is missing its private part path".into(),
                )
            })?;
            let requested_target = PathBuf::from(&durable.requested_target);
            let part_path = PathBuf::from(part_path);
            if part_path.parent() != requested_target.parent() {
                return Err(HostFileTransferStateError::InvalidState(
                    "Host Put part is not in the destination directory".into(),
                ));
            }
            let metadata = match fs::symlink_metadata(&part_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let finalizing_path = durable.finalizing_path.as_ref().ok_or_else(|| {
                        HostFileTransferStateError::InvalidState(
                            "active Host Put part disappeared without a journaled finalizing path"
                                .into(),
                        )
                    })?;
                    let finalizing_path = PathBuf::from(finalizing_path);
                    let finalizing = prepare_get_source(
                        &self.policy,
                        finalizing_path.to_string_lossy().as_ref(),
                    )
                    .map_err(|reply| {
                        HostFileTransferStateError::InvalidState(format!(
                            "journaled finalized Host Put cannot be verified: {reply:?}"
                        ))
                    })?;
                    if finalizing.2 != durable.manifest.total_size
                        || finalizing.3 != durable.manifest.final_sha256
                    {
                        return Err(HostFileTransferStateError::InvalidState(
                            "journaled finalized Host Put size/hash does not match manifest".into(),
                        ));
                    }
                    let final_path_text = finalizing.0;
                    store.puts.insert(
                        durable.manifest.transfer_id,
                        HostPutEntry {
                            sequence: durable.sequence,
                            manifest: durable.manifest,
                            descriptor: durable.descriptor,
                            phase: FileTransferPhase::Completed,
                            requested_target,
                            temp: None,
                            prefix_hasher: Sha256::new(),
                            last_chunk: None,
                            final_device_path: Some(final_path_text.to_owned()),
                            finalizing_path: None,
                        },
                    );
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(HostFileTransferStateError::InvalidState(
                    "Host Put part is not a regular non-symlink file".into(),
                ));
            }
            let expected_target =
                prepare_put_target(&self.policy, &durable.manifest).map_err(|reply| {
                    HostFileTransferStateError::InvalidState(format!(
                        "active Host Put target can no longer be prepared: {reply:?}"
                    ))
                })?;
            if expected_target != requested_target {
                return Err(HostFileTransferStateError::InvalidState(
                    "persisted Host Put target changed after restart".into(),
                ));
            }
            let part_len = metadata.len();
            if part_len > durable.manifest.total_size
                || (part_len != durable.manifest.total_size
                    && part_len % u64::from(durable.manifest.chunk_size) != 0)
                || durable.descriptor.confirmed_offset > part_len
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "Host Put part length disagrees with the persisted checkpoint".into(),
                ));
            }
            let mut file = OpenOptions::new().read(true).write(true).open(&part_path)?;
            let mut prefix_hasher = Sha256::new();
            let mut buffer = vec![0_u8; 64 * 1024];
            let mut read_total = 0_u64;
            while read_total < part_len {
                let wanted = usize::try_from((part_len - read_total).min(buffer.len() as u64))
                    .map_err(|_| {
                        HostFileTransferStateError::InvalidState("part size overflow".into())
                    })?;
                let read = file.read(&mut buffer[..wanted])?;
                if read == 0 {
                    return Err(HostFileTransferStateError::InvalidState(
                        "Host Put part ended before its metadata length".into(),
                    ));
                }
                prefix_hasher.update(&buffer[..read]);
                read_total += read as u64;
            }
            file.seek(SeekFrom::Start(part_len))?;
            let prefix_sha = digest_hex(prefix_hasher.clone().finalize());
            if part_len == durable.descriptor.confirmed_offset
                && prefix_sha != durable.descriptor.confirmed_prefix_sha256
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "Host Put part prefix hash disagrees with the persisted checkpoint".into(),
                ));
            }
            if part_len == durable.manifest.total_size
                && prefix_sha != durable.manifest.final_sha256
            {
                return Err(HostFileTransferStateError::InvalidState(
                    "Host Put completed part does not match final SHA-256".into(),
                ));
            }
            let mut descriptor = durable.descriptor;
            descriptor.checkpoint_revision = descriptor.checkpoint_revision.saturating_add(1);
            descriptor.confirmed_offset = part_len;
            descriptor.confirmed_prefix_sha256 = prefix_sha;
            descriptor.validate().map_err(|error| {
                HostFileTransferStateError::InvalidState(format!(
                    "rebuilt Host Put checkpoint is invalid: {error}"
                ))
            })?;
            let phase = if part_len == durable.manifest.total_size {
                FileTransferPhase::ReadyToFinalize
            } else {
                FileTransferPhase::Receiving
            };
            let temp_path = tempfile::TempPath::try_from_path(part_path)?;
            let temp = NamedTempFile::from_parts(file, temp_path);
            store.puts.insert(
                durable.manifest.transfer_id,
                HostPutEntry {
                    sequence: durable.sequence,
                    manifest: durable.manifest,
                    descriptor,
                    phase,
                    requested_target,
                    temp: Some(temp),
                    prefix_hasher,
                    last_chunk: None,
                    final_device_path: None,
                    finalizing_path: durable.finalizing_path.map(PathBuf::from),
                },
            );
        }
        for durable in snapshot.gets {
            let prepared = prepare_get_source(&self.policy, &durable.requested_device_path);
            let Ok((canonical_path, mut file, total_size, final_sha256)) = prepared else {
                continue;
            };
            let manifest = FileTransferManifest::new(
                durable.manifest.transfer_id,
                self.controller_id,
                self.site_id,
                self.device_id,
                FileTransferDirection::DeviceToController,
                canonical_path,
                total_size,
                durable.chunk_size,
                final_sha256,
                None,
            )
            .map_err(|error| {
                HostFileTransferStateError::InvalidState(format!(
                    "restored Host Get manifest is invalid: {error}"
                ))
            })?;
            if manifest != durable.manifest {
                continue;
            }
            file.seek(SeekFrom::Start(0))?;
            store.gets.insert(
                manifest.transfer_id,
                HostGetEntry {
                    requested_device_path: durable.requested_device_path,
                    chunk_size: durable.chunk_size,
                    manifest,
                    file,
                },
            );
        }
        if let Some(state_store) = &self.state_store {
            state_store.persist(&store)?;
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        request: FileTransferRequest,
        allow_read: bool,
        allow_write: bool,
    ) -> FileTransferReply {
        if request.validate().is_err() {
            return FileTransferReply::error(
                FileTransferErrorCode::InvalidRequest,
                "invalid bounded file transfer request",
            );
        }
        let permitted = match &request {
            FileTransferRequest::PutBegin { .. }
            | FileTransferRequest::PutChunk { .. }
            | FileTransferRequest::Finalize { .. } => self.can_put && allow_write,
            FileTransferRequest::GetBegin { .. } | FileTransferRequest::GetChunk { .. } => {
                self.can_get && allow_read
            }
            FileTransferRequest::Status { .. } | FileTransferRequest::Cancel { .. } => {
                (self.can_get && allow_read) || (self.can_put && allow_write)
            }
        };
        if !permitted || !self.policy.allows_read() {
            return FileTransferReply::error(
                FileTransferErrorCode::Denied,
                "file transfer is outside the allowed device grant",
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
            FileTransferRequest::GetBegin {
                transfer_id,
                device_path,
                chunk_size,
            } => self.get_begin(transfer_id, device_path, chunk_size),
            FileTransferRequest::GetChunk {
                transfer_id,
                offset,
            } => self.get_chunk(transfer_id, offset),
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
            if let Some(existing) = store.puts.get(&manifest.transfer_id) {
                return if existing.manifest == manifest {
                    existing.status()
                } else {
                    FileTransferReply::error(
                        FileTransferErrorCode::Conflict,
                        "TransferId is already bound to a different manifest",
                    )
                };
            }
            if store.gets.contains_key(&manifest.transfer_id) {
                return FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "TransferId is already bound to a device-to-controller transfer",
                );
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
        if let Some(existing) = store.puts.get(&transfer_id) {
            return if existing.manifest == manifest {
                existing.status()
            } else {
                FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "TransferId is already bound to a different manifest",
                )
            };
        }
        if store.gets.contains_key(&transfer_id) {
            return FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "TransferId is already bound to a device-to-controller transfer",
            );
        }
        if store.puts.len().saturating_add(store.gets.len()) >= HARD_MAX_HOST_FILE_TRANSFERS {
            return FileTransferReply::error(
                FileTransferErrorCode::Capacity,
                "Host file transfer capacity is exhausted",
            );
        }
        store.next_sequence = store.next_sequence.saturating_add(1);
        let sequence = store.next_sequence;
        store.puts.insert(
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
                finalizing_path: None,
            },
        );
        let reply = store
            .puts
            .get(&transfer_id)
            .map(HostPutEntry::status)
            .unwrap_or_else(|| transfer_io("Host transfer state failed"));
        if let Some(state_store) = &self.state_store
            && state_store.persist(&store).is_err()
        {
            store.puts.remove(&transfer_id);
            return transfer_io("Host Put state journal could not be persisted");
        }
        reply
    }

    fn put_chunk(&self, chunk: FileTransferChunk) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        let Some(entry) = store.puts.get_mut(&chunk.transfer_id) else {
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

    fn get_begin(
        &self,
        transfer_id: TransferId,
        requested_device_path: String,
        chunk_size: u32,
    ) -> FileTransferReply {
        {
            let store = match self.inner.lock() {
                Ok(store) => store,
                Err(_) => return transfer_io("Host transfer store is unavailable"),
            };
            if store.puts.contains_key(&transfer_id) {
                return FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "TransferId is already bound to a controller-to-device transfer",
                );
            }
            if let Some(existing) = store.gets.get(&transfer_id) {
                return if existing.requested_device_path == requested_device_path
                    && existing.chunk_size == chunk_size
                {
                    FileTransferReply::Manifest(existing.manifest.clone())
                } else {
                    FileTransferReply::error(
                        FileTransferErrorCode::Conflict,
                        "TransferId is already bound to a different device source",
                    )
                };
            }
        }

        let (canonical_path, mut file, total_size, final_sha256) =
            match prepare_get_source(&self.policy, &requested_device_path) {
                Ok(source) => source,
                Err(reply) => return reply,
            };
        let manifest = match FileTransferManifest::new(
            transfer_id,
            self.controller_id,
            self.site_id,
            self.device_id,
            FileTransferDirection::DeviceToController,
            canonical_path,
            total_size,
            chunk_size,
            final_sha256,
            None,
        ) {
            Ok(manifest) => manifest,
            Err(_) => {
                return FileTransferReply::error(
                    FileTransferErrorCode::InvalidRequest,
                    "Host generated an invalid device source manifest",
                );
            }
        };
        if file.seek(SeekFrom::Start(0)).is_err() {
            return transfer_io("device source seek failed");
        }
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        prune_completed(&mut store);
        if store.puts.contains_key(&transfer_id) {
            return FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "TransferId is already bound to a controller-to-device transfer",
            );
        }
        if let Some(existing) = store.gets.get(&transfer_id) {
            return if existing.requested_device_path == requested_device_path
                && existing.chunk_size == chunk_size
            {
                FileTransferReply::Manifest(existing.manifest.clone())
            } else {
                FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "TransferId is already bound to a different device source",
                )
            };
        }
        if store.puts.len().saturating_add(store.gets.len()) >= HARD_MAX_HOST_FILE_TRANSFERS {
            return FileTransferReply::error(
                FileTransferErrorCode::Capacity,
                "Host file transfer capacity is exhausted",
            );
        }
        store.gets.insert(
            transfer_id,
            HostGetEntry {
                requested_device_path,
                chunk_size,
                manifest: manifest.clone(),
                file,
            },
        );
        if let Some(state_store) = &self.state_store
            && state_store.persist(&store).is_err()
        {
            store.gets.remove(&transfer_id);
            return transfer_io("Host Get state journal could not be persisted");
        }
        FileTransferReply::Manifest(manifest)
    }

    fn get_chunk(&self, transfer_id: TransferId, offset: u64) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        let Some(entry) = store.gets.get_mut(&transfer_id) else {
            return transfer_not_found();
        };
        if entry.manifest.total_size == 0 || offset >= entry.manifest.total_size {
            return FileTransferReply::error(
                FileTransferErrorCode::OutOfOrder,
                "get chunk offset is outside the device source",
            );
        }
        if offset % u64::from(entry.manifest.chunk_size) != 0 {
            return FileTransferReply::error(
                FileTransferErrorCode::OutOfOrder,
                "get chunk offset is not on a deterministic chunk boundary",
            );
        }
        match entry.file.metadata() {
            Ok(metadata) if metadata.len() == entry.manifest.total_size => {}
            _ => {
                return FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "device source size changed during transfer",
                );
            }
        }
        let expected_len = u64::from(entry.manifest.chunk_size)
            .min(entry.manifest.total_size.saturating_sub(offset))
            as usize;
        if entry.file.seek(SeekFrom::Start(offset)).is_err() {
            return transfer_io("device source seek failed");
        }
        let mut bytes = vec![0_u8; expected_len];
        if entry.file.read_exact(&mut bytes).is_err() {
            return transfer_io("device source read failed");
        }
        match FileTransferChunk::from_bytes(transfer_id, offset, &bytes) {
            Ok(chunk) => FileTransferReply::Chunk(chunk),
            Err(_) => transfer_io("device source chunk encoding failed"),
        }
    }

    fn status(&self, transfer_id: TransferId) -> FileTransferReply {
        let store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        if let Some(entry) = store.puts.get(&transfer_id) {
            return entry.status();
        }
        store
            .gets
            .get(&transfer_id)
            .map(|entry| FileTransferReply::Manifest(entry.manifest.clone()))
            .unwrap_or_else(transfer_not_found)
    }

    fn finalize(&self, transfer_id: TransferId) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        loop {
            let (selected_target, conflict_policy) = {
                let Some(entry) = store.puts.get_mut(&transfer_id) else {
                    return if store.gets.contains_key(&transfer_id) {
                        FileTransferReply::error(
                            FileTransferErrorCode::OutOfOrder,
                            "device-to-controller transfer does not use Host finalize",
                        )
                    } else {
                        transfer_not_found()
                    };
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
                let selected_target = match entry.finalizing_path.clone() {
                    Some(path) => path,
                    None => match choose_finalizing_path(
                        &entry.requested_target,
                        entry.manifest.device_conflict_policy,
                    ) {
                        Ok(path) => path,
                        Err(reply) => return reply,
                    },
                };
                entry.finalizing_path = Some(selected_target.clone());
                (selected_target, entry.manifest.device_conflict_policy)
            };

            if let Some(state_store) = &self.state_store
                && state_store.persist(&store).is_err()
            {
                return transfer_io("Host Put finalizing path could not be persisted");
            }

            let temp = {
                let Some(entry) = store.puts.get_mut(&transfer_id) else {
                    return transfer_not_found();
                };
                let Some(temp) = entry.temp.take() else {
                    return transfer_io("file transfer part file is unavailable");
                };
                temp
            };

            match persist_final_at(temp, &selected_target, conflict_policy) {
                Ok(finalized) => {
                    let Some(entry) = store.puts.get_mut(&transfer_id) else {
                        return transfer_not_found();
                    };
                    entry.phase = FileTransferPhase::Completed;
                    entry.final_device_path = finalized.to_str().map(str::to_owned);
                    entry.finalizing_path = None;
                    if entry.final_device_path.is_none() {
                        return transfer_io("finalized device path is not valid UTF-8");
                    }
                    let reply = entry.status();
                    let _ = entry;
                    if let Some(state_store) = &self.state_store
                        && state_store.persist(&store).is_err()
                    {
                        return transfer_io("completed Host Put state could not be persisted");
                    }
                    return reply;
                }
                Err(PersistFinalError::BeforeCommit { reply, temp }) => {
                    let retry_rename = conflict_policy == Some(FileConflictPolicy::RenameIfExists)
                        && matches!(
                            &reply,
                            FileTransferReply::Error(error)
                                if error.code == FileTransferErrorCode::Conflict
                        );
                    if let Some(entry) = store.puts.get_mut(&transfer_id) {
                        entry.temp = Some(temp);
                        entry.finalizing_path = None;
                    }
                    if let Some(state_store) = &self.state_store
                        && state_store.persist(&store).is_err()
                    {
                        return transfer_io("Host Put finalizing rollback could not be persisted");
                    }
                    if retry_rename {
                        continue;
                    }
                    return reply;
                }
                Err(PersistFinalError::AfterCommit { reply, path }) => {
                    if let Some(entry) = store.puts.get_mut(&transfer_id) {
                        entry.phase = FileTransferPhase::Completed;
                        entry.final_device_path = path.to_str().map(str::to_owned);
                        entry.finalizing_path = None;
                    }
                    if let Some(state_store) = &self.state_store {
                        let _ = state_store.persist(&store);
                    }
                    return reply;
                }
            }
        }
    }

    fn cancel(&self, transfer_id: TransferId) -> FileTransferReply {
        let mut store = match self.inner.lock() {
            Ok(store) => store,
            Err(_) => return transfer_io("Host transfer store is unavailable"),
        };
        if let Some(entry) = store.puts.get(&transfer_id) {
            if entry.phase == FileTransferPhase::Completed {
                return FileTransferReply::error(
                    FileTransferErrorCode::Conflict,
                    "completed file transfer cannot be cancelled",
                );
            }
            store.puts.remove(&transfer_id);
            if let Some(state_store) = &self.state_store
                && state_store.persist(&store).is_err()
            {
                return transfer_io("Host transfer cancel could not be persisted");
            }
            return FileTransferReply::Cancelled { transfer_id };
        }
        if store.gets.remove(&transfer_id).is_some() {
            if let Some(state_store) = &self.state_store
                && state_store.persist(&store).is_err()
            {
                return transfer_io("Host transfer cancel could not be persisted");
            }
            return FileTransferReply::Cancelled { transfer_id };
        }
        transfer_not_found()
    }
}

fn prune_completed(store: &mut HostFileTransferStore) {
    while store.puts.len().saturating_add(store.gets.len()) >= HARD_MAX_HOST_FILE_TRANSFERS {
        let candidate = store
            .puts
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
        store.puts.remove(&transfer_id);
    }
}

fn prepare_get_source(
    policy: &ReadPolicy,
    requested_device_path: &str,
) -> Result<(String, File, u64, String), FileTransferReply> {
    let requested = expand_target_path(requested_device_path)
        .map_err(|_| transfer_denied("device source must be absolute or use ~/..."))?;
    if !requested.is_absolute() {
        return Err(transfer_denied(
            "device source must be absolute or use ~/...",
        ));
    }
    let metadata = fs::symlink_metadata(&requested).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FileTransferReply::error(
                FileTransferErrorCode::NotFound,
                "device source was not found",
            )
        } else {
            transfer_io("device source metadata failed")
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(transfer_denied("device source cannot be a symlink"));
    }
    if !metadata.is_file() {
        return Err(FileTransferReply::error(
            FileTransferErrorCode::Conflict,
            "device source is not a regular file",
        ));
    }
    let canonical = fs::canonicalize(&requested)
        .map_err(|_| transfer_io("device source canonicalization failed"))?;
    ensure_allowed_file(policy, &canonical)?;
    let mut file = File::open(&canonical).map_err(|_| transfer_io("device source open failed"))?;
    let mut hasher = Sha256::new();
    let mut total_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| transfer_io("device source hashing read failed"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| transfer_io("device source size overflow"))?;
    }
    let path = canonical
        .to_str()
        .ok_or_else(|| transfer_io("device source path is not valid UTF-8"))?
        .to_owned();
    Ok((path, file, total_size, digest_hex(hasher.finalize())))
}

fn ensure_allowed_file(policy: &ReadPolicy, path: &Path) -> Result<(), FileTransferReply> {
    if policy.all_filesystem {
        return Ok(());
    }
    for root in &policy.roots {
        let Ok(root) = expand_target_path(root) else {
            continue;
        };
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if path.starts_with(root) {
            return Ok(());
        }
    }
    Err(transfer_denied("device source is outside signed roots"))
}

fn prepare_put_target(
    policy: &ReadPolicy,
    manifest: &FileTransferManifest,
) -> Result<PathBuf, FileTransferReply> {
    let requested = expand_target_path(&manifest.device_path)
        .map_err(|_| transfer_denied("device destination must be absolute or use ~/..."))?;
    if !requested.is_absolute() {
        return Err(transfer_denied(
            "device destination must be absolute or use ~/...",
        ));
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
    if policy.all_filesystem {
        return Ok(());
    }
    for root in &policy.roots {
        let Ok(root) = expand_target_path(root) else {
            continue;
        };
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

fn choose_finalizing_path(
    requested_target: &Path,
    conflict: Option<FileConflictPolicy>,
) -> Result<PathBuf, FileTransferReply> {
    match conflict {
        Some(FileConflictPolicy::FailIfExists | FileConflictPolicy::ReplaceExisting) => {
            Ok(requested_target.to_path_buf())
        }
        Some(FileConflictPolicy::RenameIfExists) => {
            for attempt in 0..=HARD_MAX_RENAME_ATTEMPTS {
                let candidate = if attempt == 0 {
                    requested_target.to_path_buf()
                } else {
                    rename_candidate(requested_target, attempt).ok_or_else(|| {
                        FileTransferReply::error(
                            FileTransferErrorCode::Conflict,
                            "no bounded rename candidate is available",
                        )
                    })?
                };
                match fs::symlink_metadata(&candidate) {
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(candidate);
                    }
                    Err(_) => return Err(transfer_io("rename candidate metadata failed")),
                }
            }
            Err(FileTransferReply::error(
                FileTransferErrorCode::Conflict,
                "rename conflict attempts exhausted",
            ))
        }
        None => Err(FileTransferReply::error(
            FileTransferErrorCode::InvalidRequest,
            "device conflict policy is missing",
        )),
    }
}

fn persist_final_at(
    temp: NamedTempFile,
    final_target: &Path,
    conflict: Option<FileConflictPolicy>,
) -> Result<PathBuf, PersistFinalError> {
    match conflict {
        Some(FileConflictPolicy::FailIfExists | FileConflictPolicy::RenameIfExists) => {
            persist_noclobber(temp, final_target)
        }
        Some(FileConflictPolicy::ReplaceExisting) => {
            if let Ok(metadata) = fs::symlink_metadata(final_target)
                && (metadata.file_type().is_symlink() || metadata.is_dir())
            {
                return Err(PersistFinalError::BeforeCommit {
                    reply: transfer_denied("device destination cannot be replaced safely"),
                    temp,
                });
            }
            match temp.persist(final_target) {
                Ok(file) => match finalize_persisted(file, final_target) {
                    Ok(()) => Ok(final_target.to_path_buf()),
                    Err(reply) => Err(PersistFinalError::AfterCommit {
                        reply,
                        path: final_target.to_path_buf(),
                    }),
                },
                Err(error) => Err(PersistFinalError::BeforeCommit {
                    reply: transfer_io("atomic replace failed"),
                    temp: error.file,
                }),
            }
        }
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

fn is_valid_finalizing_path(
    requested_target: &Path,
    finalizing_path: &Path,
    conflict: Option<FileConflictPolicy>,
) -> bool {
    if requested_target.parent() != finalizing_path.parent() {
        return false;
    }
    match conflict {
        Some(FileConflictPolicy::FailIfExists | FileConflictPolicy::ReplaceExisting) => {
            finalizing_path == requested_target
        }
        Some(FileConflictPolicy::RenameIfExists) => {
            finalizing_path == requested_target
                || (1..=HARD_MAX_RENAME_ATTEMPTS).any(|attempt| {
                    rename_candidate(requested_target, attempt)
                        .as_deref()
                        .is_some_and(|candidate| candidate == finalizing_path)
                })
        }
        None => false,
    }
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
            true,
            true,
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
                true,
            )
            .await
        else {
            panic!("expected first chunk status");
        };
        assert_eq!(after_first.descriptor.confirmed_offset, 4096);
        assert_eq!(after_first.phase, FileTransferPhase::Receiving);
        let replay = service
            .execute(FileTransferRequest::PutChunk { chunk: first }, true, true)
            .await;
        assert_eq!(replay, FileTransferReply::Status(after_first.clone()));

        let status = service
            .execute(FileTransferRequest::Status { transfer_id }, true, true)
            .await;
        assert_eq!(status, FileTransferReply::Status(after_first));

        let last = FileTransferChunk::from_bytes(transfer_id, 4096, &bytes[4096..]).unwrap();
        let FileTransferReply::Status(ready) = service
            .execute(FileTransferRequest::PutChunk { chunk: last }, true, true)
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
            .execute(FileTransferRequest::Finalize { transfer_id }, true, true)
            .await
        else {
            panic!("expected completed transfer status");
        };
        assert_eq!(completed.phase, FileTransferPhase::Completed);
        assert_eq!(fs::read(root.join("target.bin")).unwrap(), bytes);
        assert_eq!(
            service
                .execute(FileTransferRequest::Finalize { transfer_id }, true, true)
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
                    true,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::NotFound
        ));
    }

    #[tokio::test]
    async fn readonly_get_is_root_bounded_replayable_and_never_grants_put() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        let bytes = vec![0x7b; 5_000];
        fs::write(&source, &bytes).unwrap();
        let outside = temp.path().join("outside.bin");
        fs::write(&outside, b"private").unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let service = HostFileTransferService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 5_000).unwrap(),
            controller_id,
            site_id,
            device_id,
            true,
            false,
        )
        .unwrap();
        let transfer_id = TransferId::new();
        let begin_request = FileTransferRequest::GetBegin {
            transfer_id,
            device_path: source.to_string_lossy().into_owned(),
            chunk_size: 4096,
        };
        let FileTransferReply::Manifest(source_manifest) =
            service.execute(begin_request.clone(), true, false).await
        else {
            panic!("expected device source manifest");
        };
        assert_eq!(source_manifest.transfer_id, transfer_id);
        assert_eq!(
            source_manifest.direction,
            FileTransferDirection::DeviceToController
        );
        assert_eq!(source_manifest.total_size, bytes.len() as u64);
        assert_eq!(source_manifest.chunk_size, 4096);
        assert_eq!(source_manifest.final_sha256, file_sha256_hex(&bytes));
        assert_eq!(source_manifest.device_conflict_policy, None);
        assert!(Path::new(&source_manifest.device_path).is_absolute());
        assert_eq!(
            service.execute(begin_request, true, false).await,
            FileTransferReply::Manifest(source_manifest.clone())
        );

        let first_request = FileTransferRequest::GetChunk {
            transfer_id,
            offset: 0,
        };
        let FileTransferReply::Chunk(first) =
            service.execute(first_request.clone(), true, false).await
        else {
            panic!("expected first device source chunk");
        };
        assert_eq!(first.offset, 0);
        assert_eq!(first.decode_bytes().unwrap(), bytes[..4096]);
        assert_eq!(
            service.execute(first_request, true, false).await,
            FileTransferReply::Chunk(first)
        );
        let FileTransferReply::Chunk(last) = service
            .execute(
                FileTransferRequest::GetChunk {
                    transfer_id,
                    offset: 4096,
                },
                true,
                false,
            )
            .await
        else {
            panic!("expected final device source chunk");
        };
        assert_eq!(last.decode_bytes().unwrap(), bytes[4096..]);

        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::GetBegin {
                        transfer_id: TransferId::new(),
                        device_path: outside.to_string_lossy().into_owned(),
                        chunk_size: 4096,
                    },
                    true,
                    false,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));
        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::GetBegin {
                        transfer_id: TransferId::new(),
                        device_path: source.to_string_lossy().into_owned(),
                        chunk_size: 4096,
                    },
                    false,
                    false,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));

        let put = manifest(
            &root,
            controller_id,
            site_id,
            device_id,
            TransferId::new(),
            b"cannot write",
            FileConflictPolicy::FailIfExists,
        );
        assert!(matches!(
            service
                .execute(FileTransferRequest::PutBegin { manifest: put }, true, false)
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));

        assert_eq!(
            service
                .execute(FileTransferRequest::Cancel { transfer_id }, true, false)
                .await,
            FileTransferReply::Cancelled { transfer_id }
        );
        assert!(matches!(
            service
                .execute(
                    FileTransferRequest::GetChunk {
                        transfer_id,
                        offset: 0,
                    },
                    true,
                    false,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::NotFound
        ));
    }

    #[tokio::test]
    async fn durable_put_and_get_survive_host_service_restart() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        let layout = StateLayout::new(temp.path().join("state"));
        fs::create_dir_all(&root).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let policy =
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 5_000).unwrap();
        let bytes = vec![0x52; 5_000];
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

        let first = HostFileTransferService::load_or_create(
            policy.clone(),
            controller_id,
            site_id,
            device_id,
            true,
            true,
            &layout,
        )
        .unwrap();
        assert!(matches!(
            first
                .execute(
                    FileTransferRequest::PutBegin {
                        manifest: put_manifest.clone(),
                    },
                    true,
                    true,
                )
                .await,
            FileTransferReply::Status(_)
        ));
        let first_chunk = FileTransferChunk::from_bytes(transfer_id, 0, &bytes[..4096]).unwrap();
        let FileTransferReply::Status(after_chunk) = first
            .execute(
                FileTransferRequest::PutChunk { chunk: first_chunk },
                true,
                true,
            )
            .await
        else {
            panic!("expected durable Put chunk status");
        };
        assert_eq!(after_chunk.descriptor.confirmed_offset, 4096);
        drop(first);

        let second = HostFileTransferService::load_or_create(
            policy.clone(),
            controller_id,
            site_id,
            device_id,
            true,
            true,
            &layout,
        )
        .unwrap();
        let FileTransferReply::Status(restored) = second
            .execute(FileTransferRequest::Status { transfer_id }, true, true)
            .await
        else {
            panic!("expected restored durable Put status");
        };
        assert_eq!(restored.phase, FileTransferPhase::Receiving);
        assert_eq!(restored.descriptor.confirmed_offset, 4096);
        assert_eq!(
            restored.descriptor.confirmed_prefix_sha256,
            file_sha256_hex(&bytes[..4096])
        );
        let last_chunk = FileTransferChunk::from_bytes(transfer_id, 4096, &bytes[4096..]).unwrap();
        assert!(matches!(
            second
                .execute(
                    FileTransferRequest::PutChunk { chunk: last_chunk },
                    true,
                    true,
                )
                .await,
            FileTransferReply::Status(FileTransferStatus {
                phase: FileTransferPhase::ReadyToFinalize,
                ..
            })
        ));
        assert!(matches!(
            second
                .execute(FileTransferRequest::Finalize { transfer_id }, true, true)
                .await,
            FileTransferReply::Status(FileTransferStatus {
                phase: FileTransferPhase::Completed,
                ..
            })
        ));
        assert_eq!(fs::read(root.join("target.bin")).unwrap(), bytes);

        let get_source = root.join("source.bin");
        let get_bytes = vec![0x33; 5_000];
        fs::write(&get_source, &get_bytes).unwrap();
        let get_id = TransferId::new();
        let begin = FileTransferRequest::GetBegin {
            transfer_id: get_id,
            device_path: get_source.to_string_lossy().into_owned(),
            chunk_size: 4096,
        };
        let FileTransferReply::Manifest(get_manifest) =
            second.execute(begin.clone(), true, true).await
        else {
            panic!("expected durable Get manifest");
        };
        drop(second);

        let third = HostFileTransferService::load_or_create(
            policy,
            controller_id,
            site_id,
            device_id,
            true,
            true,
            &layout,
        )
        .unwrap();
        assert_eq!(
            third.execute(begin, true, true).await,
            FileTransferReply::Manifest(get_manifest)
        );
        let FileTransferReply::Chunk(chunk) = third
            .execute(
                FileTransferRequest::GetChunk {
                    transfer_id: get_id,
                    offset: 0,
                },
                true,
                true,
            )
            .await
        else {
            panic!("expected restored durable Get chunk");
        };
        assert_eq!(chunk.decode_bytes().unwrap(), get_bytes[..4096]);
    }

    #[tokio::test]
    async fn durable_put_recovers_crash_after_atomic_commit_before_completed_journal() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        let layout = StateLayout::new(temp.path().join("state"));
        fs::create_dir_all(&root).unwrap();
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let bytes = vec![0x6d; 5_000];
        let transfer_id = TransferId::new();
        let manifest = manifest(
            &root,
            controller_id,
            site_id,
            device_id,
            transfer_id,
            &bytes,
            FileConflictPolicy::FailIfExists,
        );
        let requested_target = root.join("target.bin");
        fs::write(&requested_target, &bytes).unwrap();
        let mut descriptor = manifest.initial_resume_descriptor().unwrap();
        descriptor.checkpoint_revision = 2;
        descriptor.confirmed_offset = manifest.total_size;
        descriptor.confirmed_prefix_sha256 = manifest.final_sha256.clone();
        descriptor.validate().unwrap();
        let (state_store, _) =
            HostFileTransferStateStore::load(&layout, controller_id, site_id, device_id).unwrap();
        let snapshot = HostFileTransferSnapshot {
            controller_id,
            site_id,
            device_id,
            generation: 1,
            puts: vec![DurableHostPut {
                sequence: 1,
                manifest: manifest.clone(),
                descriptor,
                phase: FileTransferPhase::ReadyToFinalize,
                requested_target: requested_target.to_string_lossy().into_owned(),
                part_path: Some(root.join("missing.part").to_string_lossy().into_owned()),
                final_device_path: None,
                finalizing_path: Some(requested_target.to_string_lossy().into_owned()),
            }],
            gets: Vec::new(),
        };
        snapshot
            .validate(controller_id, site_id, device_id)
            .unwrap();
        write_host_transfer_slot(
            &state_store.root.join(HOST_TRANSFER_STATE_FILE_B),
            &snapshot,
        )
        .unwrap();

        let service = HostFileTransferService::load_or_create(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 5_000).unwrap(),
            controller_id,
            site_id,
            device_id,
            true,
            true,
            &layout,
        )
        .unwrap();
        let FileTransferReply::Status(status) = service
            .execute(FileTransferRequest::Status { transfer_id }, true, true)
            .await
        else {
            panic!("expected recovered completed Put status");
        };
        assert_eq!(status.phase, FileTransferPhase::Completed);
        let canonical_target = fs::canonicalize(&requested_target).unwrap();
        assert_eq!(
            status.final_device_path.as_deref(),
            canonical_target.to_str()
        );
        assert_eq!(fs::read(requested_target).unwrap(), bytes);
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
            true,
            true,
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
                    true,
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
                    true,
                )
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Denied
        ));

        fs::write(root.join("target.bin"), b"existing").unwrap();
        assert!(matches!(
            service
                .execute(FileTransferRequest::PutBegin { manifest: good }, true, true)
                .await,
            FileTransferReply::Error(error) if error.code == FileTransferErrorCode::Conflict
        ));
    }
}
