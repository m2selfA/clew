use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self as std_fs, File as StdFile, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use clew_core::{
    ControllerId, DeviceId, MAX_STATE_DOCUMENT_SIZE, SiteId, StateCodecError, StateLayout,
    TransferId, decode_state_json, encode_state_json,
};
use clew_transport::{
    FileConflictPolicy, FileResumeDescriptor, FileTransferChunk, FileTransferDirection,
    FileTransferErrorCode, FileTransferManifest, FileTransferPhase, FileTransferReply,
    FileTransferRequest, FileTransferStatus, MAX_FILE_CHUNK_BYTES, MIN_FILE_CHUNK_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    sync::watch,
};

use crate::{RemoteHub, RemoteHubError};

pub const HARD_MAX_CONTROLLER_FILE_TRANSFERS: usize = 16;
pub const MAX_CONTROLLER_FILE_SOURCE_PATH_BYTES: usize = 4096;
pub const MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES: usize = 4096;
const REMOTE_CANCEL_WINDOW: Duration = Duration::from_secs(30);
const CONTROLLER_TRANSFER_STATE_MAX_ENTRIES: usize = HARD_MAX_CONTROLLER_FILE_TRANSFERS;

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileGetInfo {
    pub transfer_id: TransferId,
    pub device_id: DeviceId,
    pub device_path: String,
    /// Controller-local destination. Never copied into the peer manifest.
    pub destination_path: String,
    pub phase: ControllerFileTransferPhase,
    pub chunk_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_sha256: Option<String>,
    pub confirmed_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_controller_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "direction", content = "info", rename_all = "snake_case")]
pub enum FileTransferInfo {
    Put(FilePutInfo),
    Get(FileGetInfo),
}

impl FileTransferInfo {
    #[must_use]
    pub fn transfer_id(&self) -> TransferId {
        match self {
            Self::Put(info) => info.transfer_id,
            Self::Get(info) => info.transfer_id,
        }
    }

    #[must_use]
    pub fn phase(&self) -> ControllerFileTransferPhase {
        match self {
            Self::Put(info) => info.phase,
            Self::Get(info) => info.phase,
        }
    }
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
    state_store: Option<ControllerFileTransferStateStore>,
}

#[derive(Debug)]
enum ControllerFileTransferEntry {
    Put {
        info: FilePutInfo,
        site_id: SiteId,
        conflict_policy: FileConflictPolicy,
        cancel: watch::Sender<bool>,
    },
    Get {
        info: FileGetInfo,
        site_id: SiteId,
        conflict_policy: FileConflictPolicy,
        durable_state: Option<DurableControllerGetState>,
        cancel: watch::Sender<bool>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ControllerFileTransferSnapshot {
    controller_id: ControllerId,
    generation: u64,
    transfers: Vec<DurableControllerFileTransfer>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableControllerFileTransfer {
    site_id: SiteId,
    conflict_policy: FileConflictPolicy,
    info: FileTransferInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    get_state: Option<DurableControllerGetState>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableControllerGetState {
    part_path: String,
    manifest: FileTransferManifest,
}

#[derive(Debug)]
struct ControllerFileTransferStateStore {
    layout: StateLayout,
    controller_id: ControllerId,
    generation: Mutex<u64>,
}

impl ControllerFileTransferEntry {
    fn info(&self) -> FileTransferInfo {
        match self {
            Self::Put { info, .. } => FileTransferInfo::Put(info.clone()),
            Self::Get { info, .. } => FileTransferInfo::Get(info.clone()),
        }
    }

    fn phase(&self) -> ControllerFileTransferPhase {
        match self {
            Self::Put { info, .. } => info.phase,
            Self::Get { info, .. } => info.phase,
        }
    }

    fn cancel_sender(&self) -> &watch::Sender<bool> {
        match self {
            Self::Put { cancel, .. } | Self::Get { cancel, .. } => cancel,
        }
    }

    fn mark_cancelling(&mut self) {
        match self {
            Self::Put { info, .. } => info.phase = ControllerFileTransferPhase::Cancelling,
            Self::Get { info, .. } => info.phase = ControllerFileTransferPhase::Cancelling,
        }
    }

    fn durable_record(&self) -> DurableControllerFileTransfer {
        match self {
            Self::Put {
                info,
                site_id,
                conflict_policy,
                ..
            } => DurableControllerFileTransfer {
                site_id: *site_id,
                conflict_policy: *conflict_policy,
                info: FileTransferInfo::Put(info.clone()),
                get_state: None,
            },
            Self::Get {
                info,
                site_id,
                conflict_policy,
                durable_state,
                ..
            } => DurableControllerFileTransfer {
                site_id: *site_id,
                conflict_policy: *conflict_policy,
                info: FileTransferInfo::Get(info.clone()),
                get_state: durable_state.clone(),
            },
        }
    }
}

impl ControllerFileTransferSnapshot {
    fn validate(
        &self,
        expected_controller_id: ControllerId,
    ) -> Result<(), ControllerFileTransferError> {
        if self.controller_id != expected_controller_id {
            return Err(ControllerFileTransferError::ControllerMismatch);
        }
        if self.transfers.len() > CONTROLLER_TRANSFER_STATE_MAX_ENTRIES {
            return Err(ControllerFileTransferError::InvalidPersistedState(
                "controller transfer journal exceeds the hard entry bound".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for record in &self.transfers {
            let transfer_id = record.info.transfer_id();
            if !ids.insert(transfer_id) {
                return Err(ControllerFileTransferError::InvalidPersistedState(
                    "controller transfer journal contains duplicate TransferId".into(),
                ));
            }
            match &record.info {
                FileTransferInfo::Put(info) => {
                    if record.get_state.is_some() {
                        return Err(ControllerFileTransferError::InvalidPersistedState(
                            "Controller-to-device transfer cannot carry get part state".into(),
                        ));
                    }
                    validate_start_inputs(&info.source_path, &info.device_path, info.chunk_size)?;
                    validate_persisted_progress(
                        info.total_size,
                        info.final_sha256.as_deref(),
                        info.confirmed_offset,
                    )?;
                }
                FileTransferInfo::Get(info) => {
                    validate_get_start_inputs(
                        &info.device_path,
                        &info.destination_path,
                        info.chunk_size,
                    )?;
                    validate_persisted_progress(
                        info.total_size,
                        info.final_sha256.as_deref(),
                        info.confirmed_offset,
                    )?;
                    if let Some(get_state) = &record.get_state {
                        validate_durable_get_state(
                            get_state,
                            self.controller_id,
                            record.site_id,
                            info,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl ControllerFileTransferStateStore {
    fn load(
        layout: StateLayout,
        controller_id: ControllerId,
    ) -> Result<(Self, ControllerFileTransferSnapshot), ControllerFileTransferError> {
        let mut valid = Vec::new();
        let mut first_error = None;
        let mut any_present = false;
        for path in [
            layout.controller_file_transfer_slot_a_path(),
            layout.controller_file_transfer_slot_b_path(),
        ] {
            match read_controller_transfer_slot(&path) {
                ControllerTransferSlotRead::Missing => {}
                ControllerTransferSlotRead::Valid(snapshot) => {
                    any_present = true;
                    match snapshot.validate(controller_id) {
                        Ok(()) => valid.push(snapshot),
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                ControllerTransferSlotRead::Invalid(error) => {
                    any_present = true;
                    first_error.get_or_insert(error);
                }
            }
        }
        if !valid.is_empty() {
            valid.sort_by_key(|snapshot| snapshot.generation);
            if valid.len() == 2 && valid[0].generation == valid[1].generation {
                return Err(ControllerFileTransferError::StateGenerationConflict(
                    valid[0].generation,
                ));
            }
            let snapshot = valid.pop().expect("valid transfer snapshot exists");
            let store = Self {
                layout,
                controller_id,
                generation: Mutex::new(snapshot.generation),
            };
            return Ok((store, snapshot));
        }
        if any_present {
            return Err(first_error.unwrap_or_else(|| {
                ControllerFileTransferError::InvalidPersistedState(
                    "controller transfer journal has no valid slot".into(),
                )
            }));
        }
        let snapshot = ControllerFileTransferSnapshot {
            controller_id,
            generation: 0,
            transfers: Vec::new(),
        };
        let store = Self {
            layout,
            controller_id,
            generation: Mutex::new(0),
        };
        Ok((store, snapshot))
    }

    fn persist(
        &self,
        transfers: &BTreeMap<TransferId, ControllerFileTransferEntry>,
    ) -> Result<(), ControllerFileTransferError> {
        let mut generation = self
            .generation
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        let next_generation = generation
            .checked_add(1)
            .ok_or(ControllerFileTransferError::StateGenerationOverflow)?;
        let snapshot = ControllerFileTransferSnapshot {
            controller_id: self.controller_id,
            generation: next_generation,
            transfers: transfers
                .values()
                .map(ControllerFileTransferEntry::durable_record)
                .collect(),
        };
        snapshot.validate(self.controller_id)?;
        let path = if next_generation % 2 == 0 {
            self.layout.controller_file_transfer_slot_a_path()
        } else {
            self.layout.controller_file_transfer_slot_b_path()
        };
        write_controller_transfer_slot(&path, &snapshot)?;
        *generation = next_generation;
        Ok(())
    }
}

impl Drop for ControllerFileTransferManagerInner {
    fn drop(&mut self) {
        if self.state_store.is_some() {
            return;
        }
        if let Ok(transfers) = self.transfers.get_mut() {
            for entry in transfers.values() {
                let _ = entry.cancel_sender().send(true);
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
                state_store: None,
            }),
        }
    }

    pub fn load_or_create(
        remote: RemoteHub,
        controller_id: ControllerId,
        layout: StateLayout,
    ) -> Result<Self, ControllerFileTransferError> {
        let (state_store, snapshot) =
            ControllerFileTransferStateStore::load(layout, controller_id)?;
        let manager = Self {
            inner: Arc::new(ControllerFileTransferManagerInner {
                remote,
                controller_id,
                transfers: Mutex::new(BTreeMap::new()),
                state_store: Some(state_store),
            }),
        };
        manager.restore_snapshot(snapshot)?;
        Ok(manager)
    }

    fn persist_current(&self) -> Result<(), ControllerFileTransferError> {
        let transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        if let Some(store) = &self.inner.state_store {
            store.persist(&transfers)?;
        }
        Ok(())
    }

    fn restore_snapshot(
        &self,
        snapshot: ControllerFileTransferSnapshot,
    ) -> Result<(), ControllerFileTransferError> {
        snapshot.validate(self.inner.controller_id)?;
        let mut resume_puts = Vec::new();
        let mut cancel_puts = Vec::new();
        let mut resume_gets = Vec::new();
        let mut cancel_gets = Vec::new();
        {
            let mut transfers = self
                .inner
                .transfers
                .lock()
                .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
            for record in snapshot.transfers {
                match record.info {
                    FileTransferInfo::Put(mut info) => {
                        let (cancel, cancel_rx) = watch::channel(false);
                        if info.phase == ControllerFileTransferPhase::Cancelling {
                            cancel_puts.push((info.transfer_id, info.device_id));
                        } else if !info.phase.terminal() {
                            info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                            info.error = None;
                            resume_puts.push((
                                info.transfer_id,
                                info.device_id,
                                record.site_id,
                                info.source_path.clone(),
                                info.device_path.clone(),
                                info.chunk_size,
                                record.conflict_policy,
                                cancel_rx,
                            ));
                        }
                        transfers.insert(
                            info.transfer_id,
                            ControllerFileTransferEntry::Put {
                                info,
                                site_id: record.site_id,
                                conflict_policy: record.conflict_policy,
                                cancel,
                            },
                        );
                    }
                    FileTransferInfo::Get(mut info) => {
                        let (cancel, cancel_rx) = watch::channel(false);
                        let durable_state = record.get_state.clone();
                        if info.phase == ControllerFileTransferPhase::Cancelling {
                            cancel_gets.push((
                                info.transfer_id,
                                info.device_id,
                                durable_state.as_ref().map(|state| state.part_path.clone()),
                            ));
                        } else if !info.phase.terminal() {
                            if let Some(state) = durable_state.clone() {
                                info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                                info.error = None;
                                resume_gets.push((
                                    info.transfer_id,
                                    info.device_id,
                                    record.site_id,
                                    info.device_path.clone(),
                                    info.destination_path.clone(),
                                    info.chunk_size,
                                    record.conflict_policy,
                                    state,
                                    cancel_rx,
                                ));
                            } else {
                                info.phase = ControllerFileTransferPhase::Failed;
                                info.error = Some(
                                    "Controller restart get resume awaits durable local part state"
                                        .into(),
                                );
                            }
                        }
                        transfers.insert(
                            info.transfer_id,
                            ControllerFileTransferEntry::Get {
                                info,
                                site_id: record.site_id,
                                conflict_policy: record.conflict_policy,
                                durable_state,
                                cancel,
                            },
                        );
                    }
                }
            }
        }
        self.persist_current()?;
        for (
            transfer_id,
            device_id,
            site_id,
            source_path,
            device_path,
            chunk_size,
            conflict_policy,
            cancel_rx,
        ) in resume_puts
        {
            self.spawn_put_worker(
                transfer_id,
                device_id,
                site_id,
                source_path,
                device_path,
                chunk_size,
                conflict_policy,
                cancel_rx,
            );
        }
        for (
            transfer_id,
            device_id,
            site_id,
            device_path,
            destination_path,
            chunk_size,
            conflict_policy,
            durable_state,
            cancel_rx,
        ) in resume_gets
        {
            self.spawn_get_worker(
                transfer_id,
                device_id,
                site_id,
                device_path,
                destination_path,
                chunk_size,
                conflict_policy,
                Some(durable_state),
                cancel_rx,
            );
        }
        for (transfer_id, device_id, part_path) in cancel_gets {
            if let Some(part_path) = part_path {
                let _ = std_fs::remove_file(part_path);
            }
            let weak = Arc::downgrade(&self.inner);
            let remote = self.inner.remote.clone();
            tokio::spawn(async move {
                cancel_get_remote(&weak, &remote, device_id, transfer_id).await;
                clear_get_durable_state(&weak, transfer_id);
                if let Err(error) = persist_manager_state(&weak) {
                    record_journal_warning(&weak, transfer_id, error.to_string());
                }
            });
        }
        for (transfer_id, device_id) in cancel_puts {
            let weak = Arc::downgrade(&self.inner);
            let remote = self.inner.remote.clone();
            tokio::spawn(async move {
                cancel_remote(&weak, &remote, device_id, transfer_id).await;
                if let Err(error) = persist_manager_state(&weak) {
                    record_journal_warning(&weak, transfer_id, error.to_string());
                }
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_put_worker(
        &self,
        transfer_id: TransferId,
        device_id: DeviceId,
        site_id: SiteId,
        source_path: String,
        device_path: String,
        chunk_size: u32,
        conflict_policy: FileConflictPolicy,
        cancel_rx: watch::Receiver<bool>,
    ) {
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
            if let Err(error) = persist_manager_state(&weak) {
                record_journal_warning(&weak, transfer_id, error.to_string());
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_get_worker(
        &self,
        transfer_id: TransferId,
        device_id: DeviceId,
        site_id: SiteId,
        device_path: String,
        destination_path: String,
        chunk_size: u32,
        conflict_policy: FileConflictPolicy,
        durable_state: Option<DurableControllerGetState>,
        cancel_rx: watch::Receiver<bool>,
    ) {
        let weak = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            let result = run_get_task(
                weak.clone(),
                remote.clone(),
                controller_id,
                transfer_id,
                device_id,
                site_id,
                device_path,
                PathBuf::from(destination_path),
                chunk_size,
                conflict_policy,
                durable_state,
                cancel_rx,
            )
            .await;
            match result {
                Ok(()) => {}
                Err(ControllerFileTransferError::Cancelled) => {
                    cleanup_get_local_part(&weak, transfer_id);
                    cancel_get_remote(&weak, &remote, device_id, transfer_id).await;
                    clear_get_durable_state(&weak, transfer_id);
                }
                Err(error) => {
                    cleanup_get_local_part(&weak, transfer_id);
                    set_get_failed(&weak, transfer_id, error.to_string());
                    let _ = confirm_remote_cancel(&remote, device_id, transfer_id).await;
                    clear_get_durable_state(&weak, transfer_id);
                }
            }
            if let Err(error) = persist_manager_state(&weak) {
                record_journal_warning(&weak, transfer_id, error.to_string());
            }
        });
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
            ControllerFileTransferEntry::Put {
                info: info.clone(),
                site_id,
                conflict_policy,
                cancel,
            },
        );
        drop(transfers);
        if let Err(error) = self.persist_current() {
            if let Ok(mut transfers) = self.inner.transfers.lock() {
                transfers.remove(&transfer_id);
            }
            return Err(error);
        }
        self.spawn_put_worker(
            transfer_id,
            device_id,
            site_id,
            source_path,
            device_path,
            chunk_size,
            conflict_policy,
            cancel_rx,
        );
        Ok(info)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_get(
        &self,
        device_id: DeviceId,
        site_id: SiteId,
        device_path: String,
        destination_path: String,
        chunk_size: u32,
        conflict_policy: FileConflictPolicy,
    ) -> Result<FileGetInfo, ControllerFileTransferError> {
        validate_get_start_inputs(&device_path, &destination_path, chunk_size)?;
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
        let info = FileGetInfo {
            transfer_id,
            device_id,
            device_path: device_path.clone(),
            destination_path: destination_path.clone(),
            phase: ControllerFileTransferPhase::Preparing,
            chunk_size,
            total_size: None,
            final_sha256: None,
            confirmed_offset: 0,
            final_controller_path: None,
            error: None,
        };
        let (cancel, cancel_rx) = watch::channel(false);
        transfers.insert(
            transfer_id,
            ControllerFileTransferEntry::Get {
                info: info.clone(),
                site_id,
                conflict_policy,
                durable_state: None,
                cancel,
            },
        );
        drop(transfers);
        if let Err(error) = self.persist_current() {
            if let Ok(mut transfers) = self.inner.transfers.lock() {
                transfers.remove(&transfer_id);
            }
            return Err(error);
        }

        self.spawn_get_worker(
            transfer_id,
            device_id,
            site_id,
            device_path,
            destination_path,
            chunk_size,
            conflict_policy,
            None,
            cancel_rx,
        );
        Ok(info)
    }

    pub fn status(
        &self,
        transfer_id: TransferId,
    ) -> Result<FileTransferInfo, ControllerFileTransferError> {
        self.inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?
            .get(&transfer_id)
            .map(ControllerFileTransferEntry::info)
            .ok_or(ControllerFileTransferError::NotFound(transfer_id))
    }

    pub fn cancel(
        &self,
        transfer_id: TransferId,
    ) -> Result<FileTransferInfo, ControllerFileTransferError> {
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        let entry = transfers
            .get_mut(&transfer_id)
            .ok_or(ControllerFileTransferError::NotFound(transfer_id))?;
        if !entry.phase().terminal() {
            entry.mark_cancelling();
        }
        let info = entry.info();
        drop(transfers);
        self.persist_current()?;
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
        if let Some(entry) = transfers.get_mut(&transfer_id)
            && !entry.phase().terminal()
        {
            let _ = entry.cancel_sender().send(true);
        }
        Ok(info)
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

#[allow(clippy::too_many_arguments)]
async fn run_get_task(
    manager: Weak<ControllerFileTransferManagerInner>,
    remote: RemoteHub,
    controller_id: ControllerId,
    transfer_id: TransferId,
    device_id: DeviceId,
    site_id: SiteId,
    device_path: String,
    destination_path: PathBuf,
    chunk_size: u32,
    conflict_policy: FileConflictPolicy,
    durable_state: Option<DurableControllerGetState>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), ControllerFileTransferError> {
    if is_cancelled(&mut cancel).await {
        return Err(ControllerFileTransferError::Cancelled);
    }

    let (
        mut generation,
        manifest,
        mut part_file,
        part_path,
        requested_target,
        mut descriptor,
        mut prefix_hasher,
    ) = if let Some(durable_state) = durable_state {
        let info = current_get_info(&manager, transfer_id)?;
        validate_durable_get_state(&durable_state, controller_id, site_id, &info)?;
        let destination_for_recover = destination_path.clone();
        let durable_for_recover = durable_state.clone();
        let persisted_offset = info.confirmed_offset;
        let (part_file, requested_target, descriptor, prefix_hasher) =
            tokio::task::spawn_blocking(move || {
                recover_controller_get_part(
                    &destination_for_recover,
                    conflict_policy,
                    transfer_id,
                    &durable_for_recover,
                    persisted_offset,
                )
            })
            .await
            .map_err(|_| ControllerFileTransferError::WorkerFailed)??;
        update_get_info(&manager, transfer_id, |info| {
            info.confirmed_offset = descriptor.confirmed_offset;
            info.phase = ControllerFileTransferPhase::WaitingForReconnect;
            info.error = None;
        });
        persist_manager_state(&manager)?;
        let (generation, current_manifest) = get_begin_after_generation(
            &manager,
            &remote,
            device_id,
            None,
            transfer_id,
            &device_path,
            chunk_size,
            &mut cancel,
        )
        .await?;
        if current_manifest != durable_state.manifest {
            return Err(ControllerFileTransferError::InvalidHostManifest(
                "device source manifest changed across Controller restart".into(),
            ));
        }
        update_get_info(&manager, transfer_id, |info| {
            info.phase = ControllerFileTransferPhase::Running;
        });
        (
            generation,
            durable_state.manifest,
            part_file,
            PathBuf::from(durable_state.part_path),
            requested_target,
            descriptor,
            prefix_hasher,
        )
    } else {
        let (generation, manifest) = get_begin_after_generation(
            &manager,
            &remote,
            device_id,
            None,
            transfer_id,
            &device_path,
            chunk_size,
            &mut cancel,
        )
        .await?;
        validate_get_manifest(
            &manifest,
            controller_id,
            site_id,
            device_id,
            transfer_id,
            chunk_size,
        )?;
        let destination_for_prepare = destination_path.clone();
        let (part_path, requested_target) = tokio::task::spawn_blocking(move || {
            prepare_controller_durable_destination_paths(
                &destination_for_prepare,
                conflict_policy,
                transfer_id,
            )
        })
        .await
        .map_err(|_| ControllerFileTransferError::WorkerFailed)??;
        let durable_state = DurableControllerGetState {
            part_path: part_path
                .to_str()
                .ok_or(ControllerFileTransferError::InvalidDestinationPath)?
                .to_owned(),
            manifest: manifest.clone(),
        };
        set_get_durable_state(&manager, transfer_id, Some(durable_state.clone()));
        update_get_info(&manager, transfer_id, |info| {
            info.total_size = Some(manifest.total_size);
            info.final_sha256 = Some(manifest.final_sha256.clone());
            info.phase = ControllerFileTransferPhase::Running;
        });
        persist_manager_state(&manager)?;
        let part_for_create = part_path.clone();
        let manifest_for_create = manifest.clone();
        let (part_file, descriptor, prefix_hasher) = tokio::task::spawn_blocking(move || {
            open_or_create_controller_get_part(&part_for_create, &manifest_for_create, 0, true)
        })
        .await
        .map_err(|_| ControllerFileTransferError::WorkerFailed)??;
        (
            generation,
            manifest,
            part_file,
            part_path,
            requested_target,
            descriptor,
            prefix_hasher,
        )
    };

    while descriptor.confirmed_offset < manifest.total_size {
        if is_cancelled(&mut cancel).await {
            return Err(ControllerFileTransferError::Cancelled);
        }
        let offset = descriptor.confirmed_offset;
        let request = FileTransferRequest::GetChunk {
            transfer_id,
            offset,
        };
        let reply = match remote
            .file_transfer_on_generation(device_id, generation, request.clone())
            .await?
        {
            Ok(reply) => reply,
            Err(_) => {
                update_get_info(&manager, transfer_id, |info| {
                    info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                });
                let (next_generation, next_manifest) = get_begin_after_generation(
                    &manager,
                    &remote,
                    device_id,
                    Some(generation),
                    transfer_id,
                    &device_path,
                    chunk_size,
                    &mut cancel,
                )
                .await?;
                if next_manifest != manifest {
                    return Err(ControllerFileTransferError::InvalidHostManifest(
                        "device source manifest changed after reconnect".into(),
                    ));
                }
                generation = next_generation;
                match remote
                    .file_transfer_on_generation(device_id, generation, request)
                    .await?
                {
                    Ok(reply) => reply,
                    Err(error) => return Err(error.into()),
                }
            }
        };
        let chunk = match reply {
            FileTransferReply::Chunk(chunk) => chunk,
            FileTransferReply::Error(error) => {
                return Err(ControllerFileTransferError::Remote(error.message));
            }
            _ => return Err(ControllerFileTransferError::UnexpectedReply),
        };
        if chunk.offset != offset {
            return Err(ControllerFileTransferError::InvalidHostManifest(
                "device source chunk offset changed".into(),
            ));
        }
        manifest.validate_chunk(&chunk)?;
        let bytes = chunk.decode_bytes()?;
        let expected_len =
            u64::from(manifest.chunk_size).min(manifest.total_size.saturating_sub(offset)) as usize;
        if bytes.len() != expected_len {
            return Err(ControllerFileTransferError::InvalidHostManifest(
                "device source chunk length changed".into(),
            ));
        }
        prefix_hasher.update(&bytes);
        let chunk_len = bytes.len() as u64;
        part_file = tokio::task::spawn_blocking(move || {
            write_controller_durable_chunk(part_file, offset, &bytes)
        })
        .await
        .map_err(|_| ControllerFileTransferError::WorkerFailed)??;
        descriptor.checkpoint_revision = descriptor.checkpoint_revision.saturating_add(1);
        descriptor.confirmed_offset = offset.saturating_add(chunk_len);
        descriptor.confirmed_prefix_sha256 = digest_hex(prefix_hasher.clone().finalize());
        descriptor.validate()?;
        update_get_info(&manager, transfer_id, |info| {
            info.confirmed_offset = descriptor.confirmed_offset;
            info.phase = ControllerFileTransferPhase::Running;
        });
    }

    let actual_final = digest_hex(prefix_hasher.finalize());
    if actual_final != manifest.final_sha256
        || descriptor.confirmed_prefix_sha256 != manifest.final_sha256
    {
        return Err(ControllerFileTransferError::FinalHashMismatch);
    }
    let final_path = tokio::task::spawn_blocking(move || {
        persist_controller_durable_destination(
            part_file,
            &part_path,
            &requested_target,
            conflict_policy,
        )
    })
    .await
    .map_err(|_| ControllerFileTransferError::WorkerFailed)??;
    let final_path = final_path
        .to_str()
        .ok_or(ControllerFileTransferError::InvalidDestinationPath)?
        .to_owned();
    update_get_info(&manager, transfer_id, |info| {
        info.phase = ControllerFileTransferPhase::Completed;
        info.confirmed_offset = manifest.total_size;
        info.final_controller_path = Some(final_path);
        info.error = None;
    });
    clear_get_durable_state(&manager, transfer_id);
    if !confirm_remote_cancel(&remote, device_id, transfer_id).await {
        update_get_info(&manager, transfer_id, |info| {
            info.error = Some("remote source cleanup was not confirmed".into());
        });
    }
    Ok(())
}

async fn get_begin_after_generation(
    manager: &Weak<ControllerFileTransferManagerInner>,
    remote: &RemoteHub,
    device_id: DeviceId,
    mut previous_generation: Option<u64>,
    transfer_id: TransferId,
    device_path: &str,
    chunk_size: u32,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(u64, FileTransferManifest), ControllerFileTransferError> {
    loop {
        let attempt = remote.file_transfer_attempt(
            device_id,
            previous_generation,
            FileTransferRequest::GetBegin {
                transfer_id,
                device_path: device_path.to_owned(),
                chunk_size,
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
            Ok(FileTransferReply::Manifest(manifest)) => return Ok((generation, manifest)),
            Ok(FileTransferReply::Error(error)) => {
                return Err(ControllerFileTransferError::Remote(error.message));
            }
            Ok(_) => return Err(ControllerFileTransferError::UnexpectedReply),
            Err(_) => {
                update_get_info(manager, transfer_id, |info| {
                    info.phase = ControllerFileTransferPhase::WaitingForReconnect;
                });
                previous_generation = Some(generation);
            }
        }
    }
}

fn validate_get_manifest(
    manifest: &FileTransferManifest,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    transfer_id: TransferId,
    chunk_size: u32,
) -> Result<(), ControllerFileTransferError> {
    manifest.validate()?;
    if manifest.transfer_id != transfer_id
        || manifest.controller_id != controller_id
        || manifest.site_id != site_id
        || manifest.device_id != device_id
        || manifest.direction != FileTransferDirection::DeviceToController
        || manifest.chunk_size != chunk_size
        || manifest.device_conflict_policy.is_some()
    {
        return Err(ControllerFileTransferError::InvalidHostManifest(
            "device source manifest changed transfer scope".into(),
        ));
    }
    Ok(())
}

fn current_get_info(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
) -> Result<FileGetInfo, ControllerFileTransferError> {
    let manager = manager
        .upgrade()
        .ok_or(ControllerFileTransferError::StatePoisoned)?;
    let transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
    match transfers.get(&transfer_id) {
        Some(ControllerFileTransferEntry::Get { info, .. }) => Ok(info.clone()),
        _ => Err(ControllerFileTransferError::NotFound(transfer_id)),
    }
}

fn set_get_durable_state(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    durable_state: Option<DurableControllerGetState>,
) {
    if let Some(manager) = manager.upgrade()
        && let Ok(mut transfers) = manager.transfers.lock()
        && let Some(ControllerFileTransferEntry::Get {
            durable_state: current,
            ..
        }) = transfers.get_mut(&transfer_id)
    {
        *current = durable_state;
    }
}

fn clear_get_durable_state(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
) {
    set_get_durable_state(manager, transfer_id, None);
}

fn cleanup_get_local_part(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
) {
    let part_path = manager.upgrade().and_then(|manager| {
        manager
            .transfers
            .lock()
            .ok()
            .and_then(|transfers| match transfers.get(&transfer_id) {
                Some(ControllerFileTransferEntry::Get {
                    durable_state: Some(state),
                    ..
                }) => Some(state.part_path.clone()),
                _ => None,
            })
    });
    if let Some(part_path) = part_path
        && let Err(error) = std_fs::remove_file(part_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        record_journal_warning(
            manager,
            transfer_id,
            format!("Controller get part cleanup failed: {error}"),
        );
    }
}

fn prepare_controller_durable_destination_paths(
    requested: &Path,
    conflict_policy: FileConflictPolicy,
    transfer_id: TransferId,
) -> Result<(PathBuf, PathBuf), ControllerFileTransferError> {
    if !requested.is_absolute()
        || requested.to_string_lossy().len() > MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
    {
        return Err(ControllerFileTransferError::InvalidDestinationPath);
    }
    let Some(Component::Normal(file_name)) = requested.components().next_back() else {
        return Err(ControllerFileTransferError::InvalidDestinationPath);
    };
    let Some(parent) = requested.parent() else {
        return Err(ControllerFileTransferError::InvalidDestinationPath);
    };
    let parent = std_fs::canonicalize(parent)?;
    let target = parent.join(file_name);
    if let Ok(metadata) = std_fs::symlink_metadata(&target) {
        if metadata.file_type().is_symlink() || metadata.is_dir() {
            return Err(ControllerFileTransferError::DestinationConflict);
        }
        if conflict_policy == FileConflictPolicy::FailIfExists {
            return Err(ControllerFileTransferError::DestinationConflict);
        }
    }
    let part_path = parent.join(format!(".clew-{transfer_id}.part"));
    if part_path.to_string_lossy().len() > MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES {
        return Err(ControllerFileTransferError::InvalidDestinationPath);
    }
    Ok((part_path, target))
}

fn recover_controller_get_part(
    destination_path: &Path,
    conflict_policy: FileConflictPolicy,
    transfer_id: TransferId,
    durable_state: &DurableControllerGetState,
    persisted_confirmed_offset: u64,
) -> Result<(StdFile, PathBuf, FileResumeDescriptor, Sha256), ControllerFileTransferError> {
    let (expected_part_path, requested_target) = prepare_controller_durable_destination_paths(
        destination_path,
        conflict_policy,
        transfer_id,
    )?;
    if Path::new(&durable_state.part_path) != expected_part_path {
        return Err(ControllerFileTransferError::InvalidPersistedState(
            "Controller get part path changed across restart".into(),
        ));
    }
    let (file, descriptor, hasher) = open_or_create_controller_get_part(
        &expected_part_path,
        &durable_state.manifest,
        persisted_confirmed_offset,
        persisted_confirmed_offset == 0,
    )?;
    Ok((file, requested_target, descriptor, hasher))
}

fn open_or_create_controller_get_part(
    part_path: &Path,
    manifest: &FileTransferManifest,
    persisted_confirmed_offset: u64,
    create_if_missing: bool,
) -> Result<(StdFile, FileResumeDescriptor, Sha256), ControllerFileTransferError> {
    let metadata = match std_fs::symlink_metadata(part_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut file = if let Some(metadata) = metadata {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ControllerFileTransferError::InvalidLocalCheckpoint);
        }
        OpenOptions::new().read(true).write(true).open(part_path)?
    } else if create_if_missing {
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(part_path)?
    } else {
        return Err(ControllerFileTransferError::InvalidLocalCheckpoint);
    };
    let actual_len = file.metadata()?.len();
    if actual_len > manifest.total_size
        || (actual_len != manifest.total_size && actual_len % u64::from(manifest.chunk_size) != 0)
        || persisted_confirmed_offset > actual_len
    {
        return Err(ControllerFileTransferError::InvalidLocalCheckpoint);
    }
    use std::io::{Seek as _, SeekFrom as StdSeekFrom};
    file.seek(StdSeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut remaining = actual_len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let read = std::io::Read::read(&mut file, &mut buffer[..wanted])?;
        if read == 0 {
            return Err(ControllerFileTransferError::InvalidLocalCheckpoint);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let mut descriptor = manifest.initial_resume_descriptor()?;
    if actual_len > 0 {
        descriptor.checkpoint_revision =
            1_u64.saturating_add(actual_len.div_ceil(u64::from(manifest.chunk_size)));
        descriptor.confirmed_offset = actual_len;
        descriptor.confirmed_prefix_sha256 = digest_hex(hasher.clone().finalize());
        descriptor.validate()?;
    }
    file.seek(StdSeekFrom::Start(actual_len))?;
    Ok((file, descriptor, hasher))
}

fn write_controller_durable_chunk(
    mut file: StdFile,
    offset: u64,
    bytes: &[u8],
) -> Result<StdFile, ControllerFileTransferError> {
    use std::io::{Seek as _, SeekFrom as StdSeekFrom, Write as _};
    if file.metadata()?.len() != offset {
        return Err(ControllerFileTransferError::InvalidLocalCheckpoint);
    }
    file.seek(StdSeekFrom::Start(offset))?;
    file.write_all(bytes)?;
    file.sync_data()?;
    Ok(file)
}

fn persist_controller_durable_destination(
    file: StdFile,
    part_path: &Path,
    requested_target: &Path,
    conflict_policy: FileConflictPolicy,
) -> Result<PathBuf, ControllerFileTransferError> {
    let temp_path = tempfile::TempPath::try_from_path(part_path.to_path_buf())?;
    let temp = NamedTempFile::from_parts(file, temp_path);
    persist_controller_destination(temp, requested_target, conflict_policy)
}

fn persist_controller_destination(
    mut temp: NamedTempFile,
    requested_target: &Path,
    conflict_policy: FileConflictPolicy,
) -> Result<PathBuf, ControllerFileTransferError> {
    match conflict_policy {
        FileConflictPolicy::FailIfExists => {
            let file = temp.persist_noclobber(requested_target).map_err(|error| {
                match error.error.kind() {
                    std::io::ErrorKind::AlreadyExists => {
                        ControllerFileTransferError::DestinationConflict
                    }
                    _ => ControllerFileTransferError::Io(error.error),
                }
            })?;
            finalize_controller_file(file, requested_target)?;
            Ok(requested_target.to_path_buf())
        }
        FileConflictPolicy::ReplaceExisting => {
            if let Ok(metadata) = std::fs::symlink_metadata(requested_target)
                && (metadata.file_type().is_symlink() || metadata.is_dir())
            {
                return Err(ControllerFileTransferError::DestinationConflict);
            }
            let file = temp
                .persist(requested_target)
                .map_err(|error| ControllerFileTransferError::Io(error.error))?;
            finalize_controller_file(file, requested_target)?;
            Ok(requested_target.to_path_buf())
        }
        FileConflictPolicy::RenameIfExists => {
            let mut candidate = requested_target.to_path_buf();
            for attempt in 0..=100_u32 {
                match temp.persist_noclobber(&candidate) {
                    Ok(file) => {
                        finalize_controller_file(file, &candidate)?;
                        return Ok(candidate);
                    }
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        temp = error.file;
                        candidate = controller_rename_candidate(
                            requested_target,
                            attempt.saturating_add(1),
                        )
                        .ok_or(ControllerFileTransferError::DestinationConflict)?;
                    }
                    Err(error) => return Err(ControllerFileTransferError::Io(error.error)),
                }
            }
            Err(ControllerFileTransferError::DestinationConflict)
        }
    }
}

fn controller_rename_candidate(path: &Path, attempt: u32) -> Option<PathBuf> {
    let parent = path.parent()?;
    let file_name = path.file_name()?.to_str()?;
    let candidate = parent.join(format!("{file_name}.clew-{attempt}"));
    (candidate.to_string_lossy().len() <= MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES)
        .then_some(candidate)
}

fn finalize_controller_file(
    file: std::fs::File,
    target: &Path,
) -> Result<(), ControllerFileTransferError> {
    let _ = target;
    file.sync_all()?;
    #[cfg(unix)]
    if let Some(parent) = target.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
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

async fn confirm_remote_cancel(
    remote: &RemoteHub,
    device_id: DeviceId,
    transfer_id: TransferId,
) -> bool {
    let cleanup = async {
        let (_, result) = remote
            .file_transfer_attempt(device_id, None, FileTransferRequest::Cancel { transfer_id })
            .await?;
        result
    };
    match tokio::time::timeout(REMOTE_CANCEL_WINDOW, cleanup).await {
        Ok(Ok(FileTransferReply::Cancelled { .. })) => true,
        Ok(Ok(FileTransferReply::Error(error))) => error.code == FileTransferErrorCode::NotFound,
        _ => false,
    }
}

async fn cancel_remote(
    manager: &Weak<ControllerFileTransferManagerInner>,
    remote: &RemoteHub,
    device_id: DeviceId,
    transfer_id: TransferId,
) {
    let cleanup_confirmed = confirm_remote_cancel(remote, device_id, transfer_id).await;
    update_info(manager, transfer_id, |info| {
        info.phase = ControllerFileTransferPhase::Cancelled;
        info.error = (!cleanup_confirmed).then(|| "remote part cleanup was not confirmed".into());
    });
}

async fn cancel_get_remote(
    manager: &Weak<ControllerFileTransferManagerInner>,
    remote: &RemoteHub,
    device_id: DeviceId,
    transfer_id: TransferId,
) {
    let cleanup_confirmed = confirm_remote_cancel(remote, device_id, transfer_id).await;
    update_get_info(manager, transfer_id, |info| {
        info.phase = ControllerFileTransferPhase::Cancelled;
        info.error = (!cleanup_confirmed).then(|| "remote source cleanup was not confirmed".into());
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
        && let Some(ControllerFileTransferEntry::Put { info, .. }) = transfers.get_mut(&transfer_id)
    {
        mutate(info);
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

fn update_get_info(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    mutate: impl FnOnce(&mut FileGetInfo),
) {
    if let Some(manager) = manager.upgrade()
        && let Ok(mut transfers) = manager.transfers.lock()
        && let Some(ControllerFileTransferEntry::Get { info, .. }) = transfers.get_mut(&transfer_id)
    {
        mutate(info);
    }
}

fn set_get_failed(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    update_get_info(manager, transfer_id, |info| {
        if info.phase == ControllerFileTransferPhase::Cancelling
            || info.phase == ControllerFileTransferPhase::Cancelled
        {
            return;
        }
        info.phase = ControllerFileTransferPhase::Failed;
        info.error = Some(truncate_error(error));
    });
}

fn persist_manager_state(
    manager: &Weak<ControllerFileTransferManagerInner>,
) -> Result<(), ControllerFileTransferError> {
    let Some(manager) = manager.upgrade() else {
        return Ok(());
    };
    let transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerFileTransferError::StatePoisoned)?;
    if let Some(store) = &manager.state_store {
        store.persist(&transfers)?;
    }
    Ok(())
}

fn record_journal_warning(
    manager: &Weak<ControllerFileTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    let warning = truncate_error(format!(
        "transfer state journal persistence failed: {error}"
    ));
    let Some(manager) = manager.upgrade() else {
        return;
    };
    let Ok(mut transfers) = manager.transfers.lock() else {
        return;
    };
    let Some(entry) = transfers.get_mut(&transfer_id) else {
        return;
    };
    match entry {
        ControllerFileTransferEntry::Put { info, .. } => info.error = Some(warning),
        ControllerFileTransferEntry::Get { info, .. } => info.error = Some(warning),
    }
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

fn validate_get_start_inputs(
    device_path: &str,
    destination_path: &str,
    chunk_size: u32,
) -> Result<(), ControllerFileTransferError> {
    if device_path.trim().is_empty()
        || device_path.len() > clew_transport::MAX_FILE_RESUME_PATH_BYTES
        || device_path.contains('\0')
    {
        return Err(ControllerFileTransferError::InvalidDevicePath);
    }
    if destination_path.trim().is_empty()
        || destination_path.len() > MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
        || destination_path.contains('\0')
        || !Path::new(destination_path).is_absolute()
    {
        return Err(ControllerFileTransferError::InvalidDestinationPath);
    }
    if chunk_size < MIN_FILE_CHUNK_BYTES
        || chunk_size > MAX_FILE_CHUNK_BYTES
        || !chunk_size.is_power_of_two()
    {
        return Err(ControllerFileTransferError::InvalidChunkSize(chunk_size));
    }
    Ok(())
}

fn validate_durable_get_state(
    state: &DurableControllerGetState,
    controller_id: ControllerId,
    site_id: SiteId,
    info: &FileGetInfo,
) -> Result<(), ControllerFileTransferError> {
    if state.part_path.trim().is_empty()
        || state.part_path.len() > MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
        || state.part_path.contains('\0')
        || !Path::new(&state.part_path).is_absolute()
    {
        return Err(ControllerFileTransferError::InvalidPersistedState(
            "Controller get part path is invalid".into(),
        ));
    }
    validate_get_manifest(
        &state.manifest,
        controller_id,
        site_id,
        info.device_id,
        info.transfer_id,
        info.chunk_size,
    )?;
    if info.total_size != Some(state.manifest.total_size)
        || info.final_sha256.as_ref() != Some(&state.manifest.final_sha256)
    {
        return Err(ControllerFileTransferError::InvalidPersistedState(
            "Controller get journal manifest disagrees with public transfer progress".into(),
        ));
    }
    Ok(())
}

fn prune_terminal(transfers: &mut BTreeMap<TransferId, ControllerFileTransferEntry>) {
    while transfers.len() >= HARD_MAX_CONTROLLER_FILE_TRANSFERS {
        let candidate = transfers
            .iter()
            .find_map(|(transfer_id, entry)| entry.phase().terminal().then_some(*transfer_id));
        let Some(transfer_id) = candidate else {
            break;
        };
        transfers.remove(&transfer_id);
    }
}

fn validate_persisted_progress(
    total_size: Option<u64>,
    final_sha256: Option<&str>,
    confirmed_offset: u64,
) -> Result<(), ControllerFileTransferError> {
    match (total_size, final_sha256) {
        (None, None) if confirmed_offset == 0 => Ok(()),
        (Some(total_size), Some(final_sha256)) => {
            if confirmed_offset > total_size {
                return Err(ControllerFileTransferError::InvalidPersistedState(
                    "confirmed offset exceeds persisted total size".into(),
                ));
            }
            if final_sha256.len() != 64
                || !final_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err(ControllerFileTransferError::InvalidPersistedState(
                    "persisted final SHA-256 is not canonical lowercase hex".into(),
                ));
            }
            Ok(())
        }
        _ => Err(ControllerFileTransferError::InvalidPersistedState(
            "persisted transfer progress is incomplete or inconsistent".into(),
        )),
    }
}

enum ControllerTransferSlotRead {
    Missing,
    Valid(ControllerFileTransferSnapshot),
    Invalid(ControllerFileTransferError),
}

fn read_controller_transfer_slot(path: &Path) -> ControllerTransferSlotRead {
    let mut file = match StdFile::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ControllerTransferSlotRead::Missing;
        }
        Err(error) => return ControllerTransferSlotRead::Invalid(error.into()),
    };
    let mut encoded = Vec::new();
    if let Err(error) = std::io::Read::by_ref(&mut file)
        .take((MAX_STATE_DOCUMENT_SIZE + 1) as u64)
        .read_to_end(&mut encoded)
    {
        return ControllerTransferSlotRead::Invalid(error.into());
    }
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return ControllerTransferSlotRead::Invalid(
            ControllerFileTransferError::InvalidPersistedState(
                "controller transfer journal exceeds the state document hard bound".into(),
            ),
        );
    }
    match decode_state_json(&encoded) {
        Ok(snapshot) => ControllerTransferSlotRead::Valid(snapshot),
        Err(error) => ControllerTransferSlotRead::Invalid(error.into()),
    }
}

fn write_controller_transfer_slot(
    path: &Path,
    snapshot: &ControllerFileTransferSnapshot,
) -> Result<(), ControllerFileTransferError> {
    let parent = path.parent().ok_or_else(|| {
        ControllerFileTransferError::InvalidPersistedState(
            "controller transfer journal path has no parent".into(),
        )
    })?;
    std_fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std_fs::set_permissions(parent, std_fs::Permissions::from_mode(0o700))?;
    }
    let encoded = encode_state_json(snapshot)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std_fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&encoded)?;
    file.sync_all()?;
    #[cfg(unix)]
    StdFile::open(parent)?.sync_all()?;
    Ok(())
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
    #[error("Controller file transfer state belongs to a different Controller")]
    ControllerMismatch,
    #[error("Controller file transfer persisted state is invalid: {0}")]
    InvalidPersistedState(String),
    #[error("Controller file transfer state has conflicting generation {0}")]
    StateGenerationConflict(u64),
    #[error("Controller file transfer state generation overflow")]
    StateGenerationOverflow,
    #[error("file transfer {0} was not found")]
    NotFound(TransferId),
    #[error("Controller source path is invalid or too long")]
    InvalidSourcePath,
    #[error("device destination path is invalid or too long")]
    InvalidDevicePath,
    #[error("Controller destination path is invalid, relative, or too long")]
    InvalidDestinationPath,
    #[error("Controller destination conflicts with the selected conflict policy")]
    DestinationConflict,
    #[error(
        "file chunk size must be a power of two within {MIN_FILE_CHUNK_BYTES}..={MAX_FILE_CHUNK_BYTES}, got {0}"
    )]
    InvalidChunkSize(u32),
    #[error("Controller source is not a regular non-symlink file")]
    InvalidSource,
    #[error("Controller source is too large")]
    SourceTooLarge,
    #[error("Controller download checkpoint disagrees with the local part file")]
    InvalidLocalCheckpoint,
    #[error("Host returned an invalid device source manifest: {0}")]
    InvalidHostManifest(String),
    #[error("downloaded file does not match the device source final SHA-256")]
    FinalHashMismatch,
    #[error("Controller file-transfer blocking worker failed")]
    WorkerFailed,
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
    #[error(transparent)]
    StateCodec(#[from] StateCodecError),
    #[error("Controller source I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
#[cfg(test)]
mod tests {
    use super::*;

    fn durable_put_record(root: &Path) -> (DurableControllerFileTransfer, TransferId) {
        let source = root.join("source.bin");
        std::fs::write(&source, b"controller-transfer-journal").unwrap();
        let transfer_id = TransferId::new();
        (
            DurableControllerFileTransfer {
                site_id: SiteId::new(),
                conflict_policy: FileConflictPolicy::FailIfExists,
                info: FileTransferInfo::Put(FilePutInfo {
                    transfer_id,
                    device_id: DeviceId::new(),
                    source_path: source.to_string_lossy().into_owned(),
                    device_path: "/device/target.bin".into(),
                    phase: ControllerFileTransferPhase::Preparing,
                    chunk_size: MIN_FILE_CHUNK_BYTES,
                    total_size: None,
                    final_sha256: None,
                    confirmed_offset: 0,
                    final_device_path: None,
                    error: None,
                }),
                get_state: None,
            },
            transfer_id,
        )
    }

    #[test]
    fn controller_transfer_journal_recovers_previous_valid_slot_and_rejects_bad_state() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller_id = ControllerId::new();
        let (record, transfer_id) = durable_put_record(temp.path());
        let (store, initial) =
            ControllerFileTransferStateStore::load(layout.clone(), controller_id).unwrap();
        assert_eq!(initial.generation, 0);
        assert!(initial.transfers.is_empty());

        let (cancel, _) = watch::channel(false);
        let mut transfers = BTreeMap::new();
        let FileTransferInfo::Put(info) = record.info.clone() else {
            unreachable!();
        };
        transfers.insert(
            transfer_id,
            ControllerFileTransferEntry::Put {
                info,
                site_id: record.site_id,
                conflict_policy: record.conflict_policy,
                cancel,
            },
        );
        store.persist(&transfers).unwrap();
        if let Some(ControllerFileTransferEntry::Put { info, .. }) = transfers.get_mut(&transfer_id)
        {
            info.phase = ControllerFileTransferPhase::WaitingForReconnect;
        }
        store.persist(&transfers).unwrap();
        assert!(matches!(
            ControllerFileTransferStateStore::load(layout.clone(), ControllerId::new()),
            Err(ControllerFileTransferError::ControllerMismatch)
        ));

        let newest = layout.controller_file_transfer_slot_a_path();
        std::fs::write(&newest, b"{broken").unwrap();
        let (_reloaded, recovered) =
            ControllerFileTransferStateStore::load(layout.clone(), controller_id).unwrap();
        assert_eq!(recovered.generation, 1);
        assert_eq!(recovered.transfers.len(), 1);
        assert_eq!(recovered.transfers[0].info.transfer_id(), transfer_id);

        let duplicate = ControllerFileTransferSnapshot {
            controller_id,
            generation: 7,
            transfers: vec![record.clone(), record.clone()],
        };
        assert!(matches!(
            duplicate.validate(controller_id),
            Err(ControllerFileTransferError::InvalidPersistedState(_))
        ));

        let mut invalid = record;
        let FileTransferInfo::Put(info) = &mut invalid.info else {
            unreachable!();
        };
        info.total_size = Some(10);
        info.final_sha256 = None;
        let invalid = ControllerFileTransferSnapshot {
            controller_id,
            generation: 8,
            transfers: vec![invalid],
        };
        assert!(matches!(
            invalid.validate(controller_id),
            Err(ControllerFileTransferError::InvalidPersistedState(_))
        ));
    }

    #[tokio::test]
    async fn durable_get_rebuilds_checkpoint_from_synced_part_when_journal_lags() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("state"));
        let destination_dir = temp.path().join("downloads");
        std::fs::create_dir_all(&destination_dir).unwrap();
        let destination = destination_dir.join("download.bin");
        let controller_id = ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let transfer_id = TransferId::new();
        let source = vec![0x37_u8; 8_192];
        let manifest = FileTransferManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/source.bin",
            source.len() as u64,
            MIN_FILE_CHUNK_BYTES,
            clew_transport::file_sha256_hex(&source),
            None,
        )
        .unwrap();
        let (part_path, _) = prepare_controller_durable_destination_paths(
            &destination,
            FileConflictPolicy::FailIfExists,
            transfer_id,
        )
        .unwrap();
        let (file, initial, _) =
            open_or_create_controller_get_part(&part_path, &manifest, 0, true).unwrap();
        assert_eq!(initial.confirmed_offset, 0);
        let file =
            write_controller_durable_chunk(file, 0, &source[..MIN_FILE_CHUNK_BYTES as usize])
                .unwrap();
        drop(file);

        let snapshot = ControllerFileTransferSnapshot {
            controller_id,
            generation: 1,
            transfers: vec![DurableControllerFileTransfer {
                site_id,
                conflict_policy: FileConflictPolicy::FailIfExists,
                info: FileTransferInfo::Get(FileGetInfo {
                    transfer_id,
                    device_id,
                    device_path: "/device/source.bin".into(),
                    destination_path: destination.to_string_lossy().into_owned(),
                    phase: ControllerFileTransferPhase::Running,
                    chunk_size: MIN_FILE_CHUNK_BYTES,
                    total_size: Some(source.len() as u64),
                    final_sha256: Some(manifest.final_sha256.clone()),
                    confirmed_offset: 0,
                    final_controller_path: None,
                    error: None,
                }),
                get_state: Some(DurableControllerGetState {
                    part_path: part_path.to_string_lossy().into_owned(),
                    manifest,
                }),
            }],
        };
        snapshot.validate(controller_id).unwrap();
        write_controller_transfer_slot(&layout.controller_file_transfer_slot_a_path(), &snapshot)
            .unwrap();

        let manager = ControllerFileTransferManager::load_or_create(
            RemoteHub::default(),
            controller_id,
            layout,
        )
        .unwrap();
        let recovered = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let FileTransferInfo::Get(info) = manager.status(transfer_id).unwrap() else {
                    panic!("expected durable Get projection");
                };
                if info.confirmed_offset == MIN_FILE_CHUNK_BYTES as u64 {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Controller did not rebuild durable Get checkpoint from part file");
        assert_eq!(
            recovered.phase,
            ControllerFileTransferPhase::WaitingForReconnect
        );
        assert_eq!(
            std::fs::metadata(&part_path).unwrap().len(),
            MIN_FILE_CHUNK_BYTES as u64
        );
        drop(manager);
        std::fs::remove_file(part_path).unwrap();
    }

    #[tokio::test]
    async fn durable_manager_reloads_active_put_and_fails_active_get_until_part_state_exists() {
        let temp = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("state"));
        let source = temp.path().join("source.bin");
        std::fs::write(&source, vec![0x5a; 8_192]).unwrap();
        let controller_id = ControllerId::new();
        let device_id = DeviceId::new();
        let site_id = SiteId::new();
        let first = ControllerFileTransferManager::load_or_create(
            RemoteHub::default(),
            controller_id,
            layout.clone(),
        )
        .unwrap();
        let put = first
            .start_put(
                device_id,
                site_id,
                source.to_string_lossy().into_owned(),
                "/device/target.bin".into(),
                MIN_FILE_CHUNK_BYTES,
                FileConflictPolicy::FailIfExists,
            )
            .unwrap();
        drop(first);

        let second = ControllerFileTransferManager::load_or_create(
            RemoteHub::default(),
            controller_id,
            layout.clone(),
        )
        .unwrap();
        let FileTransferInfo::Put(reloaded) = second.status(put.transfer_id).unwrap() else {
            panic!("expected durable Put projection");
        };
        assert_eq!(reloaded.transfer_id, put.transfer_id);
        assert_eq!(reloaded.source_path, put.source_path);
        assert_eq!(reloaded.device_path, put.device_path);
        assert_eq!(
            reloaded.phase,
            ControllerFileTransferPhase::WaitingForReconnect
        );

        let get_id = TransferId::new();
        let get_record = DurableControllerFileTransfer {
            site_id,
            conflict_policy: FileConflictPolicy::FailIfExists,
            info: FileTransferInfo::Get(FileGetInfo {
                transfer_id: get_id,
                device_id,
                device_path: "/device/source.bin".into(),
                destination_path: temp
                    .path()
                    .join("download.bin")
                    .to_string_lossy()
                    .into_owned(),
                phase: ControllerFileTransferPhase::Running,
                chunk_size: MIN_FILE_CHUNK_BYTES,
                total_size: Some(8_192),
                final_sha256: Some("ab".repeat(32)),
                confirmed_offset: MIN_FILE_CHUNK_BYTES as u64,
                final_controller_path: None,
                error: None,
            }),
            get_state: None,
        };
        let snapshot = ControllerFileTransferSnapshot {
            controller_id,
            generation: 20,
            transfers: vec![get_record],
        };
        snapshot.validate(controller_id).unwrap();
        write_controller_transfer_slot(&layout.controller_file_transfer_slot_a_path(), &snapshot)
            .unwrap();
        let third = ControllerFileTransferManager::load_or_create(
            RemoteHub::default(),
            controller_id,
            layout,
        )
        .unwrap();
        let FileTransferInfo::Get(reloaded_get) = third.status(get_id).unwrap() else {
            panic!("expected durable Get projection");
        };
        assert_eq!(reloaded_get.phase, ControllerFileTransferPhase::Failed);
        assert!(
            reloaded_get
                .error
                .as_deref()
                .is_some_and(|message| message.contains("durable local part state"))
        );
    }
}
