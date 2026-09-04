use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use clew_core::{ControllerId, DeviceId, SiteId, TransferId};
use clew_transport::{
    FileConflictPolicy, FileResumeDescriptor, FileTransferChunk, FileTransferDirection,
    FileTransferErrorCode, FileTransferManifest, FileTransferPhase, FileTransferReply,
    FileTransferRequest, FileTransferStatus, MAX_FILE_CHUNK_BYTES, MIN_FILE_CHUNK_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    sync::watch,
};

use crate::{RemoteHub, RemoteHubError};

pub const HARD_MAX_CONTROLLER_FILE_TRANSFERS: usize = 16;
pub const MAX_CONTROLLER_FILE_SOURCE_PATH_BYTES: usize = 4096;
const REMOTE_CANCEL_WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerFileTransferPhase {
    Preparing,
    Running,
    WaitingForReconnect,
    ReadyToFinalize,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl ControllerFileTransferPhase {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FilePutInfo {
    pub transfer_id: TransferId,
    pub device_id: DeviceId,
    /// Controller-local path. Never copied into the peer manifest.
    pub source_path: String,
    pub device_path: String,
    pub phase: ControllerFileTransferPhase,
    pub chunk_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_sha256: Option<String>,
    pub confirmed_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_device_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ControllerFileTransferManager {
    inner: Arc<ControllerFileTransferManagerInner>,
}

#[derive(Debug)]
struct ControllerFileTransferManagerInner {
    remote: RemoteHub,
    controller_id: ControllerId,
    transfers: Mutex<BTreeMap<TransferId, ControllerFileTransferEntry>>,
}

#[derive(Debug)]
struct ControllerFileTransferEntry {
    info: FilePutInfo,
    cancel: watch::Sender<bool>,
}

impl Drop for ControllerFileTransferManagerInner {
    fn drop(&mut self) {
        if let Ok(transfers) = self.transfers.get_mut() {
            for entry in transfers.values() {
                let _ = entry.cancel.send(true);
            }
        }
    }
}

impl ControllerFileTransferManager {
    #[must_use]
    pub fn new(remote: RemoteHub, controller_id: ControllerId) -> Self {
        Self {
            inner: Arc::new(ControllerFileTransferManagerInner {
                remote,
                controller_id,
                transfers: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_put(
        &self,
        device_id: DeviceId,
        site_id: SiteId,
        source_path: String,
        device_path: String,
        chunk_size: u32,
        conflict_policy: FileConflictPolicy,
    ) -> Result<FilePutInfo, ControllerFileTransferError> {
        validate_start_inputs(&source_path, &device_path, chunk_size)?;
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        prune_terminal(&mut transfers);
        if transfers.len() >= HARD_MAX_CONTROLLER_FILE_TRANSFERS {
            return Err(ControllerFileTransferError::Capacity);
        }
        let mut transfer_id = TransferId::new();
        while transfers.contains_key(&transfer_id) {
            transfer_id = TransferId::new();
        }
        let info = FilePutInfo {
            transfer_id,
            device_id,
            source_path: source_path.clone(),
            device_path: device_path.clone(),
            phase: ControllerFileTransferPhase::Preparing,
            chunk_size,
            total_size: None,
            final_sha256: None,
            confirmed_offset: 0,
            final_device_path: None,
            error: None,
        };
        let (cancel, cancel_rx) = watch::channel(false);
        transfers.insert(
            transfer_id,
            ControllerFileTransferEntry {
                info: info.clone(),
                cancel,
            },
        );
        drop(transfers);

        let weak = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            let result = run_put_task(
                weak.clone(),
                remote.clone(),
                controller_id,
                transfer_id,
                device_id,
                site_id,
                PathBuf::from(source_path),
                device_path,
                chunk_size,
                conflict_policy,
                cancel_rx,
            )
            .await;
            match result {
                Ok(()) => {}
                Err(ControllerFileTransferError::Cancelled) => {
                    cancel_remote(&weak, &remote, device_id, transfer_id).await;
                }
                Err(error) => set_failed(&weak, transfer_id, error.to_string()),
            }
        });
        Ok(info)
    }

    pub fn status(
        &self,
        transfer_id: TransferId,
    ) -> Result<FilePutInfo, ControllerFileTransferError> {
        self.inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?
            .get(&transfer_id)
            .map(|entry| entry.info.clone())
            .ok_or(ControllerFileTransferError::NotFound(transfer_id))
    }

    pub fn cancel(
        &self,
        transfer_id: TransferId,
    ) -> Result<FilePutInfo, ControllerFileTransferError> {
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        let entry = transfers
            .get_mut(&transfer_id)
            .ok_or(ControllerFileTransferError::NotFound(transfer_id))?;
        if !entry.info.phase.terminal() {
            entry.info.phase = ControllerFileTransferPhase::Cancelling;
            let _ = entry.cancel.send(true);
        }
        Ok(entry.info.clone())
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_put_task(
    manager: Weak<ControllerFileTransferManagerInner>,
    remote: RemoteHub,
    controller_id: ControllerId,
    transfer_id: TransferId,
    device_id: DeviceId,
    site_id: SiteId,
    source_path: PathBuf,
    device_path: String,
    chunk_size: u32,
    conflict_policy: FileConflictPolicy,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), ControllerFileTransferError> {
    let mut source = prepare_source(&source_path, &mut cancel).await?;
    let manifest = FileTransferManifest::new(
        transfer_id,
        controller_id,
        site_id,
        device_id,
        FileTransferDirection::ControllerToDevice,
        device_path,
        source.total_size,
        chunk_size,
        source.final_sha256.clone(),
        Some(conflict_policy),
    )?;
    update_info(&manager, transfer_id, |info| {
        info.total_size = Some(source.total_size);
        info.final_sha256 = Some(source.final_sha256.clone());
        info.phase = ControllerFileTransferPhase::Running;
    });

    if is_cancelled(&mut cancel).await {
        cancel_remote(&manager, &remote, device_id, transfer_id).await;
        return Ok(());
    }

    let (mut generation, mut status) =
        begin_or_recover(&manager, &remote, device_id, &manifest, &mut cancel).await?;
    validate_status(&manifest, None, &status)?;
    update_status_info(&manager, transfer_id, &status);

    while status.phase == FileTransferPhase::Receiving {
        if is_cancelled(&mut cancel).await {
            cancel_remote(&manager, &remote, device_id, transfer_id).await;
            return Ok(());
        }
        let offset = status.descriptor.confirmed_offset;
        source.file.seek(SeekFrom::Start(offset)).await?;
        let expected_len =
            u64::from(chunk_size).min(source.total_size.saturating_sub(offset)) as usize;
        if expected_len == 0 {
            return Err(ControllerFileTransferError::InvalidHostStatus(
                "Host reports Receiving at end of file".into(),
            ));
        }
        let mut bytes = vec![0_u8; expected_len];
        source.file.read_exact(&mut bytes).await?;
        let chunk = FileTransferChunk::from_bytes(transfer_id, offset, &bytes)?;
        let previous_descriptor = status.descriptor.clone();
        match remote
            .file_transfer_on_generation(
                device_id,
                generation,
                FileTransferRequest::PutChunk {
                    chunk: chunk.clone(),
                },
            )
            .await?
        {
            Ok(FileTransferReply::Status(next)) => {
                validate_status(&manifest, Some(&previous_descriptor), &next)?;
                if next.descriptor.confirmed_offset != offset.saturating_add(expected_len as u64) {
                    return Err(ControllerFileTransferError::InvalidHostStatus(
                        "chunk acknowledgement did not advance by exactly one chunk".into(),
                    ));
                }
                status = next;
            }
            Ok(FileTransferReply::Error(error)) => {
                return Err(ControllerFileTransferError::Remote(error.message));
            }
            Ok(_) => return Err(ControllerFileTransferError::UnexpectedReply),
            Err(_) => {
                update_info(&manager, transfer_id, |info| {
                    info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                });
                let recovered = wait_status_after_generation(
                    &remote,
                    device_id,
                    generation,
                    transfer_id,
                    &mut cancel,
                )
                .await?;
                generation = recovered.0;
                let recovered_status = recovered.1;
                validate_status(&manifest, Some(&previous_descriptor), &recovered_status)?;
                match recovered_status.descriptor.confirmed_offset {
                    confirmed if confirmed == offset => {
                        status = send_chunk_on_generation(
                            &remote,
                            device_id,
                            generation,
                            chunk,
                            &manifest,
                            &previous_descriptor,
                        )
                        .await?;
                    }
                    confirmed if confirmed == offset.saturating_add(expected_len as u64) => {
                        status = recovered_status;
                    }
                    _ => {
                        return Err(ControllerFileTransferError::InvalidHostStatus(
                            "recovered Host offset is not at this chunk boundary".into(),
                        ));
                    }
                }
            }
        }
        update_status_info(&manager, transfer_id, &status);
        update_info(&manager, transfer_id, |info| {
            if !info.phase.terminal() {
                info.phase = if status.phase == FileTransferPhase::ReadyToFinalize {
                    ControllerFileTransferPhase::ReadyToFinalize
                } else {
                    ControllerFileTransferPhase::Running
                };
            }
        });
    }

    if status.phase == FileTransferPhase::ReadyToFinalize {
        let previous_descriptor = status.descriptor.clone();
        match remote
            .file_transfer_on_generation(
                device_id,
                generation,
                FileTransferRequest::Finalize { transfer_id },
            )
            .await?
        {
            Ok(FileTransferReply::Status(next)) => {
                validate_status(&manifest, Some(&previous_descriptor), &next)?;
                status = next;
            }
            Ok(FileTransferReply::Error(error)) => {
                return Err(ControllerFileTransferError::Remote(error.message));
            }
            Ok(_) => return Err(ControllerFileTransferError::UnexpectedReply),
            Err(_) => {
                update_info(&manager, transfer_id, |info| {
                    info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                });
                let (next_generation, recovered) = wait_status_after_generation(
                    &remote,
                    device_id,
                    generation,
                    transfer_id,
                    &mut cancel,
                )
                .await?;
                generation = next_generation;
                validate_status(&manifest, Some(&previous_descriptor), &recovered)?;
                status = if recovered.phase == FileTransferPhase::Completed {
                    recovered
                } else if recovered.phase == FileTransferPhase::ReadyToFinalize {
                    match remote
                        .file_transfer_on_generation(
                            device_id,
                            generation,
                            FileTransferRequest::Finalize { transfer_id },
                        )
                        .await?
                    {
                        Ok(FileTransferReply::Status(next)) => next,
                        Ok(FileTransferReply::Error(error)) => {
                            return Err(ControllerFileTransferError::Remote(error.message));
                        }
                        Ok(_) => return Err(ControllerFileTransferError::UnexpectedReply),
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    return Err(ControllerFileTransferError::InvalidHostStatus(
                        "Host regressed from ready-to-finalize after reconnect".into(),
                    ));
                };
            }
        }
    }

    if status.phase != FileTransferPhase::Completed {
        return Err(ControllerFileTransferError::InvalidHostStatus(
            "transfer ended without Completed status".into(),
        ));
    }
    validate_status(&manifest, None, &status)?;
    update_status_info(&manager, transfer_id, &status);
    update_info(&manager, transfer_id, |info| {
        info.phase = ControllerFileTransferPhase::Completed;
        info.error = None;
    });
    Ok(())
}

struct PreparedSource {
    file: File,
    total_size: u64,
    final_sha256: String,
}

async fn prepare_source(
    path: &Path,
    cancel: &mut watch::Receiver<bool>,
) -> Result<PreparedSource, ControllerFileTransferError> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ControllerFileTransferError::InvalidSource);
    }
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut total_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return Err(ControllerFileTransferError::Cancelled);
                }
                continue;
            }
            result = file.read(&mut buffer) => result?,
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or(ControllerFileTransferError::SourceTooLarge)?;
    }
    file.seek(SeekFrom::Start(0)).await?;
    Ok(PreparedSource {
        file,
        total_size,
        final_sha256: digest_hex(hasher.finalize()),
    })
}

async fn begin_or_recover(
    manager: &Weak<ControllerFileTransferManagerInner>,
    remote: &RemoteHub,
    device_id: DeviceId,
    manifest: &FileTransferManifest,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(u64, FileTransferStatus), ControllerFileTransferError> {
    let attempt = remote.file_transfer_attempt(
        device_id,
        None,
        FileTransferRequest::PutBegin {
            manifest: manifest.clone(),
        },
    );
    let (generation, result) = tokio::select! {
        changed = cancel.changed() => {
            let _ = changed;
            return Err(ControllerFileTransferError::Cancelled);
        }
        result = attempt => result?,
    };
    match result {
        Ok(FileTransferReply::Status(status)) => Ok((generation, status)),
        Ok(FileTransferReply::Error(error)) => {
            Err(ControllerFileTransferError::Remote(error.message))
        }
        Ok(_) => Err(ControllerFileTransferError::UnexpectedReply),
        Err(_) => {
            update_info(manager, manifest.transfer_id, |info| {
                info.phase = ControllerFileTransferPhase::WaitingForReconnect;
            });
            let (next_generation, reply) = wait_reply_after_generation(
                remote,
                device_id,
                generation,
                FileTransferRequest::Status {
                    transfer_id: manifest.transfer_id,
                },
                cancel,
            )
            .await?;
            match reply {
                FileTransferReply::Status(status) => Ok((next_generation, status)),
                FileTransferReply::Error(error)
                    if error.code == FileTransferErrorCode::NotFound =>
                {
                    match remote
                        .file_transfer_on_generation(
                            device_id,
                            next_generation,
                            FileTransferRequest::PutBegin {
                                manifest: manifest.clone(),
                            },
                        )
                        .await?
                    {
                        Ok(FileTransferReply::Status(status)) => Ok((next_generation, status)),
                        Ok(FileTransferReply::Error(error)) => {
                            Err(ControllerFileTransferError::Remote(error.message))
                        }
                        Ok(_) => Err(ControllerFileTransferError::UnexpectedReply),
                        Err(error) => Err(error.into()),
                    }
                }
                FileTransferReply::Error(error) => {
                    Err(ControllerFileTransferError::Remote(error.message))
                }
                _ => Err(ControllerFileTransferError::UnexpectedReply),
            }
        }
    }
}

async fn wait_status_after_generation(
    remote: &RemoteHub,
    device_id: DeviceId,
    failed_generation: u64,
    transfer_id: TransferId,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(u64, FileTransferStatus), ControllerFileTransferError> {
    let (generation, reply) = wait_reply_after_generation(
        remote,
        device_id,
        failed_generation,
        FileTransferRequest::Status { transfer_id },
        cancel,
    )
    .await?;
    match reply {
        FileTransferReply::Status(status) => Ok((generation, status)),
        FileTransferReply::Error(error) => Err(ControllerFileTransferError::Remote(error.message)),
        _ => Err(ControllerFileTransferError::UnexpectedReply),
    }
}

async fn wait_reply_after_generation(
    remote: &RemoteHub,
    device_id: DeviceId,
    mut failed_generation: u64,
    request: FileTransferRequest,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(u64, FileTransferReply), ControllerFileTransferError> {
    loop {
        let attempt =
            remote.file_transfer_attempt(device_id, Some(failed_generation), request.clone());
        let (generation, result) = tokio::select! {
            changed = cancel.changed() => {
                let _ = changed;
                return Err(ControllerFileTransferError::Cancelled);
            }
            result = attempt => result?,
        };
        match result {
            Ok(reply) => return Ok((generation, reply)),
            Err(_) => failed_generation = generation,
        }
    }
}

async fn send_chunk_on_generation(
    remote: &RemoteHub,
    device_id: DeviceId,
    generation: u64,
    chunk: FileTransferChunk,
    manifest: &FileTransferManifest,
    previous: &FileResumeDescriptor,
) -> Result<FileTransferStatus, ControllerFileTransferError> {
    match remote
        .file_transfer_on_generation(
            device_id,
            generation,
            FileTransferRequest::PutChunk { chunk },
        )
        .await?
    {
        Ok(FileTransferReply::Status(status)) => {
            validate_status(manifest, Some(previous), &status)?;
            Ok(status)
        }
        Ok(FileTransferReply::Error(error)) => {
            Err(ControllerFileTransferError::Remote(error.message))
        }
        Ok(_) => Err(ControllerFileTransferError::UnexpectedReply),
        Err(error) => Err(error.into()),
    }
}

fn validate_status(
    manifest: &FileTransferManifest,
    previous: Option<&FileResumeDescriptor>,
    status: &FileTransferStatus,
) -> Result<(), ControllerFileTransferError> {
    status.validate()?;
    let descriptor = &status.descriptor;
    if descriptor.transfer_id != manifest.transfer_id
        || descriptor.controller_id != manifest.controller_id
        || descriptor.site_id != manifest.site_id
        || descriptor.device_id != manifest.device_id
        || descriptor.direction != manifest.direction
        || descriptor.device_path != manifest.device_path
        || descriptor.total_size != manifest.total_size
        || descriptor.final_sha256.as_ref() != Some(&manifest.final_sha256)
    {
        return Err(ControllerFileTransferError::InvalidHostStatus(
            "Host status changed transfer scope".into(),
        ));
    }
    if descriptor.confirmed_offset != manifest.total_size
        && descriptor.confirmed_offset % u64::from(manifest.chunk_size) != 0
    {
        return Err(ControllerFileTransferError::InvalidHostStatus(
            "Host status offset is not on a deterministic chunk boundary".into(),
        ));
    }
    if let Some(previous) = previous
        && descriptor != previous
    {
        descriptor.validate_successor_of(previous)?;
    }
    Ok(())
}

async fn cancel_remote(
    manager: &Weak<ControllerFileTransferManagerInner>,
    remote: &RemoteHub,
    device_id: DeviceId,
    transfer_id: TransferId,
) {
    let cleanup = async {
        let (_, result) = remote
            .file_transfer_attempt(device_id, None, FileTransferRequest::Cancel { transfer_id })
            .await?;
        result
    };
    let cleanup_confirmed = match tokio::time::timeout(REMOTE_CANCEL_WINDOW, cleanup).await {
        Ok(Ok(FileTransferReply::Cancelled { .. })) => true,
        Ok(Ok(FileTransferReply::Error(error))) => error.code == FileTransferErrorCode::NotFound,
        _ => false,
    };
    update_info(manager, transfer_id, |info| {
        info.phase = ControllerFileTransferPhase::Cancelled;
        info.error = (!cleanup_confirmed).then(|| "remote part cleanup was not confirmed".into());
    });
}

async fn is_cancelled(cancel: &mut watch::Receiver<bool>) -> bool {
    if *cancel.borrow() {
        return true;
    }
    false
}

fn update_status_info(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    status: &FileTransferStatus,
) {
    update_info(manager, transfer_id, |info| {
        info.confirmed_offset = status.descriptor.confirmed_offset;
        info.final_device_path = status.final_device_path.clone();
    });
}

fn update_info(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    mutate: impl FnOnce(&mut FilePutInfo),
) {
    if let Some(manager) = manager.upgrade()
        && let Ok(mut transfers) = manager.transfers.lock()
        && let Some(entry) = transfers.get_mut(&transfer_id)
    {
        mutate(&mut entry.info);
    }
}

fn set_failed(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    update_info(manager, transfer_id, |info| {
        if info.phase == ControllerFileTransferPhase::Cancelling
            || info.phase == ControllerFileTransferPhase::Cancelled
        {
            return;
        }
        info.phase = ControllerFileTransferPhase::Failed;
        info.error = Some(truncate_error(error));
    });
}

fn truncate_error(mut error: String) -> String {
    const MAX_ERROR_BYTES: usize = 2048;
    if error.len() <= MAX_ERROR_BYTES {
        return error;
    }
    let mut boundary = MAX_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error.truncate(boundary);
    error
}

fn validate_start_inputs(
    source_path: &str,
    device_path: &str,
    chunk_size: u32,
) -> Result<(), ControllerFileTransferError> {
    if source_path.trim().is_empty()
        || source_path.len() > MAX_CONTROLLER_FILE_SOURCE_PATH_BYTES
        || source_path.contains('\0')
        || !Path::new(source_path).is_absolute()
    {
        return Err(ControllerFileTransferError::InvalidSourcePath);
    }
    if device_path.trim().is_empty()
        || device_path.len() > clew_transport::MAX_FILE_RESUME_PATH_BYTES
        || device_path.contains('\0')
    {
        return Err(ControllerFileTransferError::InvalidDevicePath);
    }
    if chunk_size < MIN_FILE_CHUNK_BYTES
        || chunk_size > MAX_FILE_CHUNK_BYTES
        || !chunk_size.is_power_of_two()
    {
        return Err(ControllerFileTransferError::InvalidChunkSize(chunk_size));
    }
    Ok(())
}

fn prune_terminal(transfers: &mut BTreeMap<TransferId, ControllerFileTransferEntry>) {
    while transfers.len() >= HARD_MAX_CONTROLLER_FILE_TRANSFERS {
        let candidate = transfers
            .iter()
            .find_map(|(transfer_id, entry)| entry.info.phase.terminal().then_some(*transfer_id));
        let Some(transfer_id) = candidate else {
            break;
        };
        transfers.remove(&transfer_id);
    }
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Error)]
pub enum ControllerFileTransferError {
    #[error("Controller file transfer state is poisoned")]
    StatePoisoned,
    #[error("Controller file transfer capacity is exhausted")]
    Capacity,
    #[error("file transfer {0} was not found")]
    NotFound(TransferId),
    #[error("Controller source path is invalid or too long")]
    InvalidSourcePath,
    #[error("device destination path is invalid or too long")]
    InvalidDevicePath,
    #[error(
        "file chunk size must be a power of two within {MIN_FILE_CHUNK_BYTES}..={MAX_FILE_CHUNK_BYTES}, got {0}"
    )]
    InvalidChunkSize(u32),
    #[error("Controller source is not a regular non-symlink file")]
    InvalidSource,
    #[error("Controller source is too large")]
    SourceTooLarge,
    #[error("file transfer was cancelled")]
    Cancelled,
    #[error("Host returned an invalid file transfer status: {0}")]
    InvalidHostStatus(String),
    #[error("Host rejected file transfer: {0}")]
    Remote(String),
    #[error("Host returned an unexpected file transfer reply")]
    UnexpectedReply,
    #[error(transparent)]
    RemoteHub(#[from] RemoteHubError),
    #[error(transparent)]
    Protocol(#[from] clew_transport::FileTransferError),
    #[error(transparent)]
    Resume(#[from] clew_transport::FileResumeError),
    #[error("Controller source I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
