use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use clew_core::{
    ControllerId, DeviceId, MAX_STATE_DOCUMENT_SIZE, SiteId, StateCodecError, StateLayout,
    TransferId, decode_state_json, encode_state_json,
};
use clew_host::scan_directory_tree;
use clew_transport::{
    DirectoryConflictPolicy, DirectoryTreeEntryKind, DirectoryTreeGetScope, DirectoryTreeManifest,
    DirectoryTreeReply, DirectoryTreeRequest, FileConflictPolicy, FileTransferDirection,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

use crate::{
    ControllerFileTransferError, ControllerFileTransferManager, ControllerFileTransferPhase,
    FileTransferInfo, RemoteHub, RemoteHubError,
};

pub const HARD_MAX_CONTROLLER_DIRECTORY_TRANSFERS: usize = 8;
const CONTROLLER_DIRECTORY_TRANSFER_STATE_MAX_ENTRIES: usize =
    HARD_MAX_CONTROLLER_DIRECTORY_TRANSFERS;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerDirectoryTransferPhase {
    Preparing,
    Running,
    WaitingForReconnect,
    Finalizing,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl ControllerDirectoryTransferPhase {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryPutInfo {
    pub transfer_id: TransferId,
    pub device_id: DeviceId,
    /// Controller-local directory. Never copied into the peer manifest.
    pub source_path: String,
    pub device_root: String,
    pub phase: ControllerDirectoryTransferPhase,
    pub chunk_size: u32,
    #[serde(default)]
    pub total_file_bytes: u64,
    #[serde(default)]
    pub confirmed_file_bytes: u64,
    #[serde(default)]
    pub total_files: u32,
    #[serde(default)]
    pub completed_files: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_relative_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_file_transfer_id: Option<TransferId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_device_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryGetInfo {
    pub transfer_id: TransferId,
    pub device_id: DeviceId,
    pub device_root: String,
    /// Controller-local destination directory. Never copied into the peer manifest.
    pub destination_path: String,
    pub phase: ControllerDirectoryTransferPhase,
    pub chunk_size: u32,
    #[serde(default)]
    pub total_file_bytes: u64,
    #[serde(default)]
    pub confirmed_file_bytes: u64,
    #[serde(default)]
    pub total_files: u32,
    #[serde(default)]
    pub completed_files: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_relative_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_file_transfer_id: Option<TransferId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_destination_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "direction", content = "info", rename_all = "snake_case")]
pub enum DirectoryTransferInfo {
    Put(DirectoryPutInfo),
    Get(DirectoryGetInfo),
}

impl DirectoryTransferInfo {
    #[must_use]
    fn transfer_id(&self) -> TransferId {
        match self {
            Self::Put(info) => info.transfer_id,
            Self::Get(info) => info.transfer_id,
        }
    }

    #[must_use]
    fn device_id(&self) -> DeviceId {
        match self {
            Self::Put(info) => info.device_id,
            Self::Get(info) => info.device_id,
        }
    }

    fn phase(&self) -> ControllerDirectoryTransferPhase {
        match self {
            Self::Put(info) => info.phase,
            Self::Get(info) => info.phase,
        }
    }

    fn set_phase(&mut self, phase: ControllerDirectoryTransferPhase) {
        match self {
            Self::Put(info) => info.phase = phase,
            Self::Get(info) => info.phase = phase,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ControllerDirectoryTransferManager {
    inner: Arc<ControllerDirectoryTransferManagerInner>,
}

#[derive(Debug)]
struct ControllerDirectoryTransferManagerInner {
    remote: RemoteHub,
    file_transfers: ControllerFileTransferManager,
    controller_id: ControllerId,
    transfers: Mutex<BTreeMap<TransferId, DirectoryTransferEntry>>,
    state_store: Option<ControllerDirectoryTransferStateStore>,
}

#[derive(Debug)]
struct DirectoryTransferEntry {
    info: DirectoryTransferInfo,
    site_id: SiteId,
    durable_state: Option<DurableControllerDirectoryState>,
    cancel: watch::Sender<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ControllerDirectoryTransferSnapshot {
    controller_id: ControllerId,
    generation: u64,
    transfers: Vec<DurableControllerDirectoryTransfer>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableControllerDirectoryTransfer {
    site_id: SiteId,
    info: DirectoryTransferInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<DurableControllerDirectoryState>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "direction", content = "state", rename_all = "snake_case")]
enum DurableControllerDirectoryState {
    Put {
        manifest: DirectoryTreeManifest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        staging_device_root: Option<String>,
    },
    Get {
        manifest: DirectoryTreeManifest,
        staging_controller_root: String,
        final_controller_root: String,
        #[serde(default)]
        source_verified: bool,
    },
}

#[derive(Debug)]
struct ControllerDirectoryTransferStateStore {
    layout: StateLayout,
    controller_id: ControllerId,
    generation: Mutex<u64>,
}

impl DirectoryTransferEntry {
    fn durable_record(&self) -> DurableControllerDirectoryTransfer {
        DurableControllerDirectoryTransfer {
            site_id: self.site_id,
            info: self.info.clone(),
            state: self.durable_state.clone(),
        }
    }
}

impl ControllerDirectoryTransferSnapshot {
    fn validate(
        &self,
        expected_controller_id: ControllerId,
    ) -> Result<(), ControllerDirectoryTransferError> {
        if self.controller_id != expected_controller_id {
            return Err(ControllerDirectoryTransferError::ControllerMismatch);
        }
        if self.transfers.len() > CONTROLLER_DIRECTORY_TRANSFER_STATE_MAX_ENTRIES {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "controller directory transfer journal exceeds the hard entry bound".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for record in &self.transfers {
            let transfer_id = record.info.transfer_id();
            if !ids.insert(transfer_id) {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "controller directory transfer journal contains duplicate TransferId".into(),
                ));
            }
            validate_durable_directory_record(record, self.controller_id)?;
        }
        Ok(())
    }
}

impl ControllerDirectoryTransferStateStore {
    fn load(
        layout: StateLayout,
        controller_id: ControllerId,
    ) -> Result<(Self, ControllerDirectoryTransferSnapshot), ControllerDirectoryTransferError> {
        let mut valid = Vec::new();
        let mut first_error = None;
        let mut any_present = false;
        for path in [
            layout.controller_directory_transfer_slot_a_path(),
            layout.controller_directory_transfer_slot_b_path(),
        ] {
            match read_controller_directory_transfer_slot(&path) {
                ControllerDirectoryTransferSlotRead::Missing => {}
                ControllerDirectoryTransferSlotRead::Valid(snapshot) => {
                    any_present = true;
                    match snapshot.validate(controller_id) {
                        Ok(()) => valid.push(snapshot),
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                ControllerDirectoryTransferSlotRead::Invalid(error) => {
                    any_present = true;
                    first_error.get_or_insert(error);
                }
            }
        }
        if !valid.is_empty() {
            valid.sort_by_key(|snapshot| snapshot.generation);
            if valid.len() == 2 && valid[0].generation == valid[1].generation {
                return Err(ControllerDirectoryTransferError::StateGenerationConflict(
                    valid[0].generation,
                ));
            }
            let snapshot = valid
                .pop()
                .expect("valid directory transfer snapshot exists");
            let store = Self {
                layout,
                controller_id,
                generation: Mutex::new(snapshot.generation),
            };
            return Ok((store, snapshot));
        }
        if any_present {
            return Err(first_error.unwrap_or_else(|| {
                ControllerDirectoryTransferError::InvalidPersistedState(
                    "controller directory transfer journal has no valid slot".into(),
                )
            }));
        }
        let snapshot = ControllerDirectoryTransferSnapshot {
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
        transfers: &BTreeMap<TransferId, DirectoryTransferEntry>,
    ) -> Result<(), ControllerDirectoryTransferError> {
        let mut generation = self
            .generation
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        let next_generation = generation
            .checked_add(1)
            .ok_or(ControllerDirectoryTransferError::StateGenerationOverflow)?;
        let snapshot = ControllerDirectoryTransferSnapshot {
            controller_id: self.controller_id,
            generation: next_generation,
            transfers: transfers
                .values()
                .map(DirectoryTransferEntry::durable_record)
                .collect(),
        };
        snapshot.validate(self.controller_id)?;
        let path = if next_generation % 2 == 0 {
            self.layout.controller_directory_transfer_slot_a_path()
        } else {
            self.layout.controller_directory_transfer_slot_b_path()
        };
        write_controller_directory_transfer_slot(&path, &snapshot)?;
        *generation = next_generation;
        Ok(())
    }
}

impl ControllerDirectoryTransferManager {
    #[must_use]
    pub fn new(
        remote: RemoteHub,
        file_transfers: ControllerFileTransferManager,
        controller_id: ControllerId,
    ) -> Self {
        Self {
            inner: Arc::new(ControllerDirectoryTransferManagerInner {
                remote,
                file_transfers,
                controller_id,
                transfers: Mutex::new(BTreeMap::new()),
                state_store: None,
            }),
        }
    }

    pub fn load_or_create(
        remote: RemoteHub,
        file_transfers: ControllerFileTransferManager,
        controller_id: ControllerId,
        layout: StateLayout,
    ) -> Result<Self, ControllerDirectoryTransferError> {
        let (state_store, snapshot) =
            ControllerDirectoryTransferStateStore::load(layout, controller_id)?;
        let manager = Self {
            inner: Arc::new(ControllerDirectoryTransferManagerInner {
                remote,
                file_transfers,
                controller_id,
                transfers: Mutex::new(BTreeMap::new()),
                state_store: Some(state_store),
            }),
        };
        manager.restore_snapshot(snapshot)?;
        Ok(manager)
    }

    fn persist_current(&self) -> Result<(), ControllerDirectoryTransferError> {
        let transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        if let Some(store) = &self.inner.state_store {
            store.persist(&transfers)?;
        }
        Ok(())
    }

    fn restore_snapshot(
        &self,
        snapshot: ControllerDirectoryTransferSnapshot,
    ) -> Result<(), ControllerDirectoryTransferError> {
        snapshot.validate(self.inner.controller_id)?;
        let mut resume_puts = Vec::new();
        let mut resume_gets = Vec::new();
        let mut resume_cancels = Vec::new();
        {
            let mut transfers = self
                .inner
                .transfers
                .lock()
                .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
            for record in snapshot.transfers {
                let (cancel, cancel_rx) = watch::channel(false);
                let mut info = record.info;
                if info.phase() == ControllerDirectoryTransferPhase::Cancelling {
                    resume_cancels.push((
                        info.transfer_id(),
                        info.device_id(),
                        info.clone(),
                        record.state.clone(),
                    ));
                } else if !info.phase().terminal() {
                    if info.phase() != ControllerDirectoryTransferPhase::Finalizing {
                        info.set_phase(ControllerDirectoryTransferPhase::WaitingForReconnect);
                    }
                    match &mut info {
                        DirectoryTransferInfo::Put(info) => info.error = None,
                        DirectoryTransferInfo::Get(info) => info.error = None,
                    }
                    match &info {
                        DirectoryTransferInfo::Put(put) => resume_puts.push((
                            put.transfer_id,
                            put.device_id,
                            record.site_id,
                            put.source_path.clone(),
                            put.device_root.clone(),
                            put.chunk_size,
                            record.state.clone(),
                            cancel_rx,
                        )),
                        DirectoryTransferInfo::Get(get) => resume_gets.push((
                            get.transfer_id,
                            get.device_id,
                            record.site_id,
                            get.device_root.clone(),
                            get.destination_path.clone(),
                            get.chunk_size,
                            record.state.clone(),
                            cancel_rx,
                        )),
                    }
                }
                transfers.insert(
                    info.transfer_id(),
                    DirectoryTransferEntry {
                        info,
                        site_id: record.site_id,
                        durable_state: record.state,
                        cancel,
                    },
                );
            }
        }
        self.persist_current()?;
        for (
            transfer_id,
            device_id,
            site_id,
            source_path,
            device_root,
            chunk_size,
            durable_state,
            cancel_rx,
        ) in resume_puts
        {
            self.spawn_put_worker(
                transfer_id,
                device_id,
                site_id,
                source_path,
                device_root,
                chunk_size,
                durable_state,
                cancel_rx,
            );
        }
        for (
            transfer_id,
            device_id,
            site_id,
            device_root,
            destination_path,
            chunk_size,
            durable_state,
            cancel_rx,
        ) in resume_gets
        {
            self.spawn_get_worker(
                transfer_id,
                device_id,
                site_id,
                device_root,
                destination_path,
                chunk_size,
                durable_state,
                cancel_rx,
            );
        }
        for (transfer_id, device_id, info, durable_state) in resume_cancels {
            let weak = Arc::downgrade(&self.inner);
            let remote = self.inner.remote.clone();
            let file_transfers = self.inner.file_transfers.clone();
            tokio::spawn(async move {
                recover_directory_cancel(
                    &weak,
                    &remote,
                    &file_transfers,
                    transfer_id,
                    device_id,
                    &info,
                    durable_state.as_ref(),
                )
                .await;
                persist_directory_manager_state_or_warn(&weak, transfer_id);
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
        device_root: String,
        chunk_size: u32,
        durable_state: Option<DurableControllerDirectoryState>,
        cancel_rx: watch::Receiver<bool>,
    ) {
        let manager = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let file_transfers = self.inner.file_transfers.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            run_directory_put(
                manager.clone(),
                remote,
                file_transfers,
                controller_id,
                transfer_id,
                device_id,
                site_id,
                source_path,
                device_root,
                chunk_size,
                durable_state,
                cancel_rx,
            )
            .await;
            persist_directory_manager_state_or_warn(&manager, transfer_id);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_get_worker(
        &self,
        transfer_id: TransferId,
        device_id: DeviceId,
        site_id: SiteId,
        device_root: String,
        destination_path: String,
        chunk_size: u32,
        durable_state: Option<DurableControllerDirectoryState>,
        cancel_rx: watch::Receiver<bool>,
    ) {
        let manager = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let file_transfers = self.inner.file_transfers.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            run_directory_get(
                manager.clone(),
                remote,
                file_transfers,
                controller_id,
                transfer_id,
                device_id,
                site_id,
                device_root,
                destination_path,
                chunk_size,
                durable_state,
                cancel_rx,
            )
            .await;
            persist_directory_manager_state_or_warn(&manager, transfer_id);
        });
    }

    pub fn start_put(
        &self,
        device_id: DeviceId,
        site_id: SiteId,
        source_path: String,
        device_root: String,
        chunk_size: u32,
    ) -> Result<DirectoryPutInfo, ControllerDirectoryTransferError> {
        validate_start_inputs(&source_path, &device_root, chunk_size)?;
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        prune_terminal(&mut transfers);
        if transfers.len() >= HARD_MAX_CONTROLLER_DIRECTORY_TRANSFERS {
            return Err(ControllerDirectoryTransferError::Capacity);
        }
        let mut transfer_id = TransferId::new();
        while transfers.contains_key(&transfer_id) {
            transfer_id = TransferId::new();
        }
        let info = DirectoryPutInfo {
            transfer_id,
            device_id,
            source_path: source_path.clone(),
            device_root: device_root.clone(),
            phase: ControllerDirectoryTransferPhase::Preparing,
            chunk_size,
            total_file_bytes: 0,
            confirmed_file_bytes: 0,
            total_files: 0,
            completed_files: 0,
            current_relative_path: None,
            current_file_transfer_id: None,
            final_device_root: None,
            error: None,
        };
        let (cancel, cancel_rx) = watch::channel(false);
        transfers.insert(
            transfer_id,
            DirectoryTransferEntry {
                info: DirectoryTransferInfo::Put(info.clone()),
                site_id,
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
        self.spawn_put_worker(
            transfer_id,
            device_id,
            site_id,
            source_path,
            device_root,
            chunk_size,
            None,
            cancel_rx,
        );
        Ok(info)
    }

    pub fn start_get(
        &self,
        device_id: DeviceId,
        site_id: SiteId,
        device_root: String,
        destination_path: String,
        chunk_size: u32,
    ) -> Result<DirectoryGetInfo, ControllerDirectoryTransferError> {
        validate_get_start_inputs(&device_root, &destination_path, chunk_size)?;
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        prune_terminal(&mut transfers);
        if transfers.len() >= HARD_MAX_CONTROLLER_DIRECTORY_TRANSFERS {
            return Err(ControllerDirectoryTransferError::Capacity);
        }
        let mut transfer_id = TransferId::new();
        while transfers.contains_key(&transfer_id) {
            transfer_id = TransferId::new();
        }
        let info = DirectoryGetInfo {
            transfer_id,
            device_id,
            device_root: device_root.clone(),
            destination_path: destination_path.clone(),
            phase: ControllerDirectoryTransferPhase::Preparing,
            chunk_size,
            total_file_bytes: 0,
            confirmed_file_bytes: 0,
            total_files: 0,
            completed_files: 0,
            current_relative_path: None,
            current_file_transfer_id: None,
            final_destination_path: None,
            error: None,
        };
        let (cancel, cancel_rx) = watch::channel(false);
        transfers.insert(
            transfer_id,
            DirectoryTransferEntry {
                info: DirectoryTransferInfo::Get(info.clone()),
                site_id,
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
            device_root,
            destination_path,
            chunk_size,
            None,
            cancel_rx,
        );
        Ok(info)
    }

    pub fn status(
        &self,
        transfer_id: TransferId,
    ) -> Result<DirectoryTransferInfo, ControllerDirectoryTransferError> {
        self.inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?
            .get(&transfer_id)
            .map(|entry| entry.info.clone())
            .ok_or(ControllerDirectoryTransferError::NotFound(transfer_id))
    }

    pub fn cancel(
        &self,
        transfer_id: TransferId,
    ) -> Result<DirectoryTransferInfo, ControllerDirectoryTransferError> {
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        let entry = transfers
            .get_mut(&transfer_id)
            .ok_or(ControllerDirectoryTransferError::NotFound(transfer_id))?;
        if matches!(
            entry.info.phase(),
            ControllerDirectoryTransferPhase::Preparing
                | ControllerDirectoryTransferPhase::Running
                | ControllerDirectoryTransferPhase::WaitingForReconnect
        ) {
            entry
                .info
                .set_phase(ControllerDirectoryTransferPhase::Cancelling);
        }
        let info = entry.info.clone();
        drop(transfers);
        self.persist_current()?;
        let mut transfers = self
            .inner
            .transfers
            .lock()
            .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
        if let Some(entry) = transfers.get_mut(&transfer_id)
            && !entry.info.phase().terminal()
        {
            let _ = entry.cancel.send(true);
        }
        Ok(info)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryCancelWake {
    Requested,
    OwnerDropped,
}

#[allow(clippy::too_many_arguments)]
async fn run_directory_put(
    manager: Weak<ControllerDirectoryTransferManagerInner>,
    remote: RemoteHub,
    file_transfers: ControllerFileTransferManager,
    controller_id: ControllerId,
    transfer_id: TransferId,
    device_id: DeviceId,
    site_id: SiteId,
    source_path: String,
    device_root: String,
    chunk_size: u32,
    durable_state: Option<DurableControllerDirectoryState>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut canonical_root = None;
    let (manifest, persisted_staging_root) = match durable_state {
        Some(DurableControllerDirectoryState::Put {
            manifest,
            staging_device_root,
        }) => (manifest, staging_device_root),
        Some(DurableControllerDirectoryState::Get { .. }) => {
            set_failed(
                &manager,
                transfer_id,
                "persisted directory transfer direction changed".into(),
            );
            return;
        }
        None => {
            let source = PathBuf::from(&source_path);
            let scan = match tokio::task::spawn_blocking(move || scan_directory_tree(&source)).await
            {
                Ok(Ok(scan)) => scan,
                Ok(Err(error)) => {
                    set_failed(&manager, transfer_id, error.to_string());
                    return;
                }
                Err(_) => {
                    set_failed(&manager, transfer_id, "directory scan worker failed".into());
                    return;
                }
            };
            if *cancel.borrow() {
                set_cancelled(&manager, transfer_id);
                persist_directory_manager_state_or_warn(&manager, transfer_id);
                return;
            }
            canonical_root = Some(scan.canonical_root().to_path_buf());
            let total_file_bytes = scan.total_file_bytes;
            let total_files = scan
                .entries
                .iter()
                .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
                .count() as u32;
            let manifest = match scan.into_manifest(
                transfer_id,
                controller_id,
                site_id,
                device_id,
                FileTransferDirection::ControllerToDevice,
                device_root.clone(),
                Some(DirectoryConflictPolicy::FailIfExists),
            ) {
                Ok(manifest) => manifest,
                Err(error) => {
                    set_failed(&manager, transfer_id, error.to_string());
                    return;
                }
            };
            update_info(&manager, transfer_id, |info| {
                info.total_file_bytes = total_file_bytes;
                info.total_files = total_files;
            });
            if let Err(error) = set_durable_state(
                &manager,
                transfer_id,
                DurableControllerDirectoryState::Put {
                    manifest: manifest.clone(),
                    staging_device_root: None,
                },
            )
            .and_then(|()| persist_directory_manager_state(&manager))
            {
                set_failed(&manager, transfer_id, error.to_string());
                return;
            }
            (manifest, None)
        }
    };
    let files = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
        .collect::<Vec<_>>();

    let prepare = remote.directory_tree(
        device_id,
        DirectoryTreeRequest::PreparePut {
            manifest: manifest.clone(),
        },
    );
    tokio::pin!(prepare);
    let prepare_reply = tokio::select! {
        reply = &mut prepare => reply,
        wake = wait_for_cancel(&mut cancel) => {
            if wake == DirectoryCancelWake::Requested {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_cancelled(&manager, transfer_id);
                persist_directory_manager_state_or_warn(&manager, transfer_id);
            }
            return;
        }
    };
    let staging_root = match prepare_reply {
        Ok(DirectoryTreeReply::Prepared {
            staging_device_root,
            ..
        }) => {
            if let Some(expected) = &persisted_staging_root
                && expected != &staging_device_root
            {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(
                    &manager,
                    transfer_id,
                    "Host changed the persisted directory staging root".into(),
                );
                return;
            }
            if let Err(error) = set_durable_state(
                &manager,
                transfer_id,
                DurableControllerDirectoryState::Put {
                    manifest: manifest.clone(),
                    staging_device_root: Some(staging_device_root.clone()),
                },
            )
            .and_then(|()| persist_directory_manager_state(&manager))
            {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(&manager, transfer_id, error.to_string());
                return;
            }
            staging_device_root
        }
        Ok(DirectoryTreeReply::Completed {
            final_device_root, ..
        }) => {
            update_info(&manager, transfer_id, |info| {
                info.phase = ControllerDirectoryTransferPhase::Completed;
                info.confirmed_file_bytes = info.total_file_bytes;
                info.completed_files = info.total_files;
                info.current_relative_path = None;
                info.current_file_transfer_id = None;
                info.final_device_root = Some(final_device_root);
                info.error = None;
            });
            persist_directory_manager_state_or_warn(&manager, transfer_id);
            return;
        }
        Ok(DirectoryTreeReply::Error(error)) => {
            set_failed(&manager, transfer_id, error.message);
            return;
        }
        Ok(reply) => {
            set_failed(
                &manager,
                transfer_id,
                format!("unexpected directory prepare reply: {reply:?}"),
            );
            return;
        }
        Err(error) => {
            set_failed(&manager, transfer_id, error.to_string());
            return;
        }
    };

    let initial_info = match current_put_info(&manager, transfer_id) {
        Ok(info) => info,
        Err(error) => {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(&manager, transfer_id, error.to_string());
            return;
        }
    };
    if initial_info.completed_files > files.len() as u32 {
        cancel_remote_directory(&remote, device_id, &manifest).await;
        set_failed(
            &manager,
            transfer_id,
            "persisted directory completed-file prefix exceeds manifest".into(),
        );
        return;
    }
    let mut completed_bytes: u64 = files
        .iter()
        .take(initial_info.completed_files as usize)
        .map(|entry| entry.size)
        .sum();

    if initial_info.completed_files < files.len() as u32 && canonical_root.is_none() {
        let source = PathBuf::from(&source_path);
        let scan = match tokio::task::spawn_blocking(move || scan_directory_tree(&source)).await {
            Ok(Ok(scan)) => scan,
            Ok(Err(error)) => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(&manager, transfer_id, error.to_string());
                return;
            }
            Err(_) => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(&manager, transfer_id, "directory scan worker failed".into());
                return;
            }
        };
        if scan.entries != manifest.entries || scan.total_file_bytes != manifest.total_file_bytes {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(
                &manager,
                transfer_id,
                "Controller directory source changed across process restart".into(),
            );
            return;
        }
        canonical_root = Some(scan.canonical_root().to_path_buf());
    }

    if initial_info.completed_files < files.len() as u32 {
        update_info(&manager, transfer_id, |info| {
            info.phase = ControllerDirectoryTransferPhase::Running;
            if info.current_file_transfer_id.is_none() {
                info.confirmed_file_bytes = completed_bytes;
            }
            info.error = None;
        });
        if let Err(error) = persist_directory_manager_state(&manager) {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(&manager, transfer_id, error.to_string());
            return;
        }
    }

    for (file_index, entry) in files
        .iter()
        .enumerate()
        .skip(initial_info.completed_files as usize)
    {
        if *cancel.borrow() {
            if let Ok(info) = current_put_info(&manager, transfer_id)
                && let Some(child_id) = info.current_file_transfer_id
            {
                let _ = file_transfers.cancel(child_id);
            }
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_cancelled(&manager, transfer_id);
            persist_directory_manager_state_or_warn(&manager, transfer_id);
            return;
        }
        let Some(canonical_root) = canonical_root.as_ref() else {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(
                &manager,
                transfer_id,
                "directory source root is unavailable".into(),
            );
            return;
        };
        let local_path = join_relative(canonical_root, &entry.relative_path);
        let Some(local_path) = local_path.to_str().map(str::to_owned) else {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(
                &manager,
                transfer_id,
                "Controller source path is not valid UTF-8".into(),
            );
            return;
        };
        let device_path = join_device_relative(&staging_root, &entry.relative_path);
        let outer = match current_put_info(&manager, transfer_id) {
            Ok(info) => info,
            Err(error) => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(&manager, transfer_id, error.to_string());
                return;
            }
        };
        let child_id = match (
            outer.current_relative_path.as_deref(),
            outer.current_file_transfer_id,
        ) {
            (Some(relative), Some(child_id)) if relative == entry.relative_path => child_id,
            (None, None) => {
                let child_id = TransferId::new();
                update_info(&manager, transfer_id, |info| {
                    info.current_relative_path = Some(entry.relative_path.clone());
                    info.current_file_transfer_id = Some(child_id);
                    info.confirmed_file_bytes = completed_bytes;
                });
                if let Err(error) = persist_directory_manager_state(&manager) {
                    cancel_remote_directory(&remote, device_id, &manifest).await;
                    set_failed(&manager, transfer_id, error.to_string());
                    return;
                }
                child_id
            }
            _ => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(
                    &manager,
                    transfer_id,
                    "persisted directory child does not match the next manifest file".into(),
                );
                return;
            }
        };
        let child = match file_transfers.status(child_id) {
            Ok(FileTransferInfo::Put(info)) => {
                if info.device_id != device_id
                    || info.source_path != local_path
                    || info.device_path != device_path
                    || info.chunk_size != chunk_size
                {
                    cancel_remote_directory(&remote, device_id, &manifest).await;
                    set_failed(
                        &manager,
                        transfer_id,
                        "persisted directory child Put changed transfer scope".into(),
                    );
                    return;
                }
                info
            }
            Ok(FileTransferInfo::Get(_)) => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(
                    &manager,
                    transfer_id,
                    "persisted directory child transfer direction changed".into(),
                );
                return;
            }
            Err(ControllerFileTransferError::NotFound(_)) => {
                match file_transfers.start_put_with_transfer_id(
                    child_id,
                    device_id,
                    site_id,
                    local_path,
                    device_path,
                    chunk_size,
                    FileConflictPolicy::FailIfExists,
                ) {
                    Ok(info) => info,
                    Err(error) => {
                        cancel_remote_directory(&remote, device_id, &manifest).await;
                        set_failed(&manager, transfer_id, error.to_string());
                        return;
                    }
                }
            }
            Err(error) => {
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_failed(&manager, transfer_id, error.to_string());
                return;
            }
        };

        loop {
            if *cancel.borrow() {
                let _ = file_transfers.cancel(child_id);
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_cancelled(&manager, transfer_id);
                persist_directory_manager_state_or_warn(&manager, transfer_id);
                return;
            }
            match file_transfers.status(child_id) {
                Ok(FileTransferInfo::Put(info)) => {
                    update_info(&manager, transfer_id, |directory| {
                        directory.confirmed_file_bytes =
                            completed_bytes.saturating_add(info.confirmed_offset.min(entry.size));
                    });
                    match info.phase {
                        ControllerFileTransferPhase::Completed => {
                            completed_bytes = completed_bytes.saturating_add(entry.size);
                            update_info(&manager, transfer_id, |directory| {
                                directory.completed_files = (file_index + 1) as u32;
                                directory.confirmed_file_bytes = completed_bytes;
                                directory.current_relative_path = None;
                                directory.current_file_transfer_id = None;
                            });
                            if let Err(error) = persist_directory_manager_state(&manager) {
                                cancel_remote_directory(&remote, device_id, &manifest).await;
                                set_failed(&manager, transfer_id, error.to_string());
                                return;
                            }
                            break;
                        }
                        ControllerFileTransferPhase::Failed => {
                            cancel_remote_directory(&remote, device_id, &manifest).await;
                            set_failed(
                                &manager,
                                transfer_id,
                                info.error.unwrap_or_else(|| "child file Put failed".into()),
                            );
                            return;
                        }
                        ControllerFileTransferPhase::Cancelled => {
                            cancel_remote_directory(&remote, device_id, &manifest).await;
                            set_cancelled(&manager, transfer_id);
                            persist_directory_manager_state_or_warn(&manager, transfer_id);
                            return;
                        }
                        _ => {}
                    }
                }
                Ok(FileTransferInfo::Get(_)) => {
                    cancel_remote_directory(&remote, device_id, &manifest).await;
                    set_failed(
                        &manager,
                        transfer_id,
                        "child transfer direction changed".into(),
                    );
                    return;
                }
                Err(error) => {
                    cancel_remote_directory(&remote, device_id, &manifest).await;
                    set_failed(&manager, transfer_id, error.to_string());
                    return;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                changed = cancel.changed() => {
                    match changed {
                        Ok(()) if *cancel.borrow() => {
                            let _ = file_transfers.cancel(child.transfer_id);
                            cancel_remote_directory(&remote, device_id, &manifest).await;
                            set_cancelled(&manager, transfer_id);
                            persist_directory_manager_state_or_warn(&manager, transfer_id);
                            return;
                        }
                        Ok(()) => {}
                        Err(_) => return,
                    }
                }
            }
        }
    }

    if *cancel.borrow() {
        cancel_remote_directory(&remote, device_id, &manifest).await;
        set_cancelled(&manager, transfer_id);
        persist_directory_manager_state_or_warn(&manager, transfer_id);
        return;
    }
    update_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Finalizing;
        info.confirmed_file_bytes = info.total_file_bytes;
        info.completed_files = info.total_files;
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
    if let Err(error) = persist_directory_manager_state(&manager) {
        cancel_remote_directory(&remote, device_id, &manifest).await;
        set_failed(&manager, transfer_id, error.to_string());
        return;
    }
    match remote
        .directory_tree(
            device_id,
            DirectoryTreeRequest::FinalizePut {
                manifest: manifest.clone(),
            },
        )
        .await
    {
        Ok(DirectoryTreeReply::Completed {
            final_device_root, ..
        }) => {
            update_info(&manager, transfer_id, |info| {
                info.phase = ControllerDirectoryTransferPhase::Completed;
                info.final_device_root = Some(final_device_root);
                info.error = None;
            });
            persist_directory_manager_state_or_warn(&manager, transfer_id);
        }
        Ok(DirectoryTreeReply::Error(error)) => {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(&manager, transfer_id, error.message);
        }
        Ok(reply) => {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(
                &manager,
                transfer_id,
                format!("unexpected directory finalize reply: {reply:?}"),
            );
        }
        Err(error) => {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_failed(&manager, transfer_id, error.to_string());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_directory_get(
    manager: Weak<ControllerDirectoryTransferManagerInner>,
    remote: RemoteHub,
    file_transfers: ControllerFileTransferManager,
    controller_id: ControllerId,
    transfer_id: TransferId,
    device_id: DeviceId,
    site_id: SiteId,
    device_root: String,
    destination_path: String,
    chunk_size: u32,
    durable_state: Option<DurableControllerDirectoryState>,
    mut cancel: watch::Receiver<bool>,
) {
    let scope = match DirectoryTreeGetScope::new(
        transfer_id,
        controller_id,
        site_id,
        device_id,
        device_root.clone(),
    ) {
        Ok(scope) => scope,
        Err(error) => {
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
    };
    let current_phase = current_get_info(&manager, transfer_id)
        .map(|info| info.phase)
        .unwrap_or(ControllerDirectoryTransferPhase::Preparing);
    let recovered = match durable_state {
        Some(DurableControllerDirectoryState::Get {
            manifest,
            staging_controller_root,
            final_controller_root,
            source_verified,
        }) => Some((
            manifest,
            PathBuf::from(staging_controller_root),
            PathBuf::from(final_controller_root),
            source_verified,
        )),
        Some(DurableControllerDirectoryState::Put { .. }) => {
            set_get_failed(
                &manager,
                transfer_id,
                "persisted directory transfer direction changed".into(),
            );
            return;
        }
        None => None,
    };
    let was_recovered = recovered.is_some();

    if let Some((manifest, staging_root, final_root, true)) = recovered.as_ref()
        && current_phase == ControllerDirectoryTransferPhase::Finalizing
    {
        let staging = staging_root.clone();
        let final_path = final_root.clone();
        let manifest_for_recovery = manifest.clone();
        match tokio::task::spawn_blocking(move || {
            recover_local_get_commit(&staging, &final_path, &manifest_for_recovery)
        })
        .await
        {
            Ok(Ok(Some(final_path))) => {
                update_get_info(&manager, transfer_id, |info| {
                    info.phase = ControllerDirectoryTransferPhase::Completed;
                    info.confirmed_file_bytes = info.total_file_bytes;
                    info.completed_files = info.total_files;
                    info.current_relative_path = None;
                    info.current_file_transfer_id = None;
                    info.final_destination_path = Some(final_path);
                    info.error = None;
                });
                persist_directory_manager_state_or_warn(&manager, transfer_id);
                return;
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
            Err(_) => {
                set_get_failed(
                    &manager,
                    transfer_id,
                    ControllerDirectoryTransferError::WorkerFailed.to_string(),
                );
                return;
            }
        }
    }

    let source_already_verified = recovered
        .as_ref()
        .is_some_and(|(_, _, _, source_verified)| *source_verified);
    let host_manifest = if source_already_verified {
        recovered.as_ref().map(|state| state.0.clone())
    } else {
        let prepare = remote.directory_tree(
            device_id,
            DirectoryTreeRequest::PrepareGet {
                scope: scope.clone(),
            },
        );
        tokio::pin!(prepare);
        let prepare_reply = tokio::select! {
            reply = &mut prepare => reply,
            wake = wait_for_cancel(&mut cancel) => {
                if wake == DirectoryCancelWake::Requested {
                    if let Some((_, staging, _, _)) = &recovered {
                        cleanup_local_directory(staging);
                    }
                    set_get_cancelled(&manager, transfer_id);
                    persist_directory_manager_state_or_warn(&manager, transfer_id);
                }
                return;
            }
        };
        match prepare_reply {
            Ok(DirectoryTreeReply::Manifest { manifest }) => Some(manifest),
            Ok(DirectoryTreeReply::Error(error)) => {
                set_get_failed(&manager, transfer_id, error.message);
                return;
            }
            Ok(reply) => {
                set_get_failed(
                    &manager,
                    transfer_id,
                    format!("unexpected directory Get prepare reply: {reply:?}"),
                );
                return;
            }
            Err(error) => {
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
        }
    };
    let Some(host_manifest) = host_manifest else {
        set_get_failed(
            &manager,
            transfer_id,
            "directory manifest is unavailable".into(),
        );
        return;
    };
    if host_manifest.validate().is_err()
        || host_manifest.transfer_id != transfer_id
        || host_manifest.controller_id != controller_id
        || host_manifest.site_id != site_id
        || host_manifest.device_id != device_id
        || host_manifest.direction != FileTransferDirection::DeviceToController
        || host_manifest.device_root != device_root
        || host_manifest.device_conflict_policy.is_some()
    {
        set_get_failed(
            &manager,
            transfer_id,
            "Host returned a directory manifest with changed transfer scope".into(),
        );
        return;
    }

    let (manifest, staging_root, final_root, mut source_verified) = match recovered {
        Some((manifest, staging_root, final_root, source_verified)) => {
            if host_manifest != manifest {
                set_get_failed(
                    &manager,
                    transfer_id,
                    "device directory source changed across Controller restart".into(),
                );
                return;
            }
            (manifest, staging_root, final_root, source_verified)
        }
        None => {
            let destination = PathBuf::from(&destination_path);
            let destination_for_paths = destination.clone();
            let paths = match tokio::task::spawn_blocking(move || {
                local_get_paths(&destination_for_paths, transfer_id)
            })
            .await
            {
                Ok(Ok(paths)) => paths,
                Ok(Err(error)) => {
                    set_get_failed(&manager, transfer_id, error.to_string());
                    return;
                }
                Err(_) => {
                    set_get_failed(
                        &manager,
                        transfer_id,
                        ControllerDirectoryTransferError::WorkerFailed.to_string(),
                    );
                    return;
                }
            };
            let (staging_root, final_root) = paths;
            let total_files = host_manifest
                .entries
                .iter()
                .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
                .count() as u32;
            update_get_info(&manager, transfer_id, |info| {
                info.total_file_bytes = host_manifest.total_file_bytes;
                info.total_files = total_files;
            });
            let Some(staging_string) = staging_root.to_str().map(str::to_owned) else {
                set_get_failed(
                    &manager,
                    transfer_id,
                    ControllerDirectoryTransferError::InvalidDestinationPath.to_string(),
                );
                return;
            };
            let Some(final_string) = final_root.to_str().map(str::to_owned) else {
                set_get_failed(
                    &manager,
                    transfer_id,
                    ControllerDirectoryTransferError::InvalidDestinationPath.to_string(),
                );
                return;
            };
            if let Err(error) = set_durable_state(
                &manager,
                transfer_id,
                DurableControllerDirectoryState::Get {
                    manifest: host_manifest.clone(),
                    staging_controller_root: staging_string,
                    final_controller_root: final_string,
                    source_verified: false,
                },
            )
            .and_then(|()| persist_directory_manager_state(&manager))
            {
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
            (host_manifest, staging_root, final_root, false)
        }
    };
    let files = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
        .collect::<Vec<_>>();
    let initial_info = match current_get_info(&manager, transfer_id) {
        Ok(info) => info,
        Err(error) => {
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
    };
    if initial_info.completed_files > files.len() as u32 {
        set_get_failed(
            &manager,
            transfer_id,
            "persisted directory completed-file prefix exceeds manifest".into(),
        );
        return;
    }
    let allow_create_staging = initial_info.completed_files == 0
        && initial_info.current_file_transfer_id.is_none()
        && !source_verified;
    let staging_for_prepare = staging_root.clone();
    let final_for_prepare = final_root.clone();
    let manifest_for_prepare = manifest.clone();
    match tokio::task::spawn_blocking(move || {
        ensure_local_get_staging(
            &staging_for_prepare,
            &final_for_prepare,
            &manifest_for_prepare,
            allow_create_staging,
            was_recovered,
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
        Err(_) => {
            set_get_failed(
                &manager,
                transfer_id,
                ControllerDirectoryTransferError::WorkerFailed.to_string(),
            );
            return;
        }
    }
    if *cancel.borrow() {
        if let Some(child_id) = initial_info.current_file_transfer_id {
            let _ = file_transfers.cancel(child_id);
        }
        cleanup_local_directory(&staging_root);
        set_get_cancelled(&manager, transfer_id);
        persist_directory_manager_state_or_warn(&manager, transfer_id);
        return;
    }

    let mut completed_bytes: u64 = files
        .iter()
        .take(initial_info.completed_files as usize)
        .map(|entry| entry.size)
        .sum();
    if initial_info.completed_files < files.len() as u32 {
        update_get_info(&manager, transfer_id, |info| {
            info.phase = ControllerDirectoryTransferPhase::Running;
            if info.current_file_transfer_id.is_none() {
                info.confirmed_file_bytes = completed_bytes;
            }
            info.error = None;
        });
        if let Err(error) = persist_directory_manager_state(&manager) {
            cleanup_local_directory(&staging_root);
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
    }

    for (file_index, entry) in files
        .iter()
        .enumerate()
        .skip(initial_info.completed_files as usize)
    {
        if *cancel.borrow() {
            if let Ok(info) = current_get_info(&manager, transfer_id)
                && let Some(child_id) = info.current_file_transfer_id
            {
                let _ = file_transfers.cancel(child_id);
            }
            cleanup_local_directory(&staging_root);
            set_get_cancelled(&manager, transfer_id);
            persist_directory_manager_state_or_warn(&manager, transfer_id);
            return;
        }
        let device_path = join_device_relative(&manifest.device_root, &entry.relative_path);
        let local_path = join_relative(&staging_root, &entry.relative_path);
        let Some(local_path) = local_path.to_str().map(str::to_owned) else {
            cleanup_local_directory(&staging_root);
            set_get_failed(
                &manager,
                transfer_id,
                "Controller directory staging path is not valid UTF-8".into(),
            );
            return;
        };
        let outer = match current_get_info(&manager, transfer_id) {
            Ok(info) => info,
            Err(error) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
        };
        let child_id = match (
            outer.current_relative_path.as_deref(),
            outer.current_file_transfer_id,
        ) {
            (Some(relative), Some(child_id)) if relative == entry.relative_path => child_id,
            (None, None) => {
                let child_id = TransferId::new();
                update_get_info(&manager, transfer_id, |info| {
                    info.current_relative_path = Some(entry.relative_path.clone());
                    info.current_file_transfer_id = Some(child_id);
                    info.confirmed_file_bytes = completed_bytes;
                });
                if let Err(error) = persist_directory_manager_state(&manager) {
                    cleanup_local_directory(&staging_root);
                    set_get_failed(&manager, transfer_id, error.to_string());
                    return;
                }
                child_id
            }
            _ => {
                cleanup_local_directory(&staging_root);
                set_get_failed(
                    &manager,
                    transfer_id,
                    "persisted directory child does not match the next manifest file".into(),
                );
                return;
            }
        };
        let child = match file_transfers.status(child_id) {
            Ok(FileTransferInfo::Get(info)) => {
                if info.device_id != device_id
                    || info.device_path != device_path
                    || info.destination_path != local_path
                    || info.chunk_size != chunk_size
                {
                    cleanup_local_directory(&staging_root);
                    set_get_failed(
                        &manager,
                        transfer_id,
                        "persisted directory child Get changed transfer scope".into(),
                    );
                    return;
                }
                info
            }
            Ok(FileTransferInfo::Put(_)) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(
                    &manager,
                    transfer_id,
                    "persisted directory child transfer direction changed".into(),
                );
                return;
            }
            Err(ControllerFileTransferError::NotFound(_)) => {
                match file_transfers.start_get_with_transfer_id(
                    child_id,
                    device_id,
                    site_id,
                    device_path,
                    local_path,
                    chunk_size,
                    FileConflictPolicy::FailIfExists,
                ) {
                    Ok(info) => info,
                    Err(error) => {
                        cleanup_local_directory(&staging_root);
                        set_get_failed(&manager, transfer_id, error.to_string());
                        return;
                    }
                }
            }
            Err(error) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
        };

        loop {
            if *cancel.borrow() {
                let _ = file_transfers.cancel(child_id);
                cleanup_local_directory(&staging_root);
                set_get_cancelled(&manager, transfer_id);
                persist_directory_manager_state_or_warn(&manager, transfer_id);
                return;
            }
            match file_transfers.status(child_id) {
                Ok(FileTransferInfo::Get(info)) => {
                    update_get_info(&manager, transfer_id, |directory| {
                        directory.confirmed_file_bytes =
                            completed_bytes.saturating_add(info.confirmed_offset.min(entry.size));
                    });
                    match info.phase {
                        ControllerFileTransferPhase::Completed => {
                            completed_bytes = completed_bytes.saturating_add(entry.size);
                            update_get_info(&manager, transfer_id, |directory| {
                                directory.completed_files = (file_index + 1) as u32;
                                directory.confirmed_file_bytes = completed_bytes;
                                directory.current_relative_path = None;
                                directory.current_file_transfer_id = None;
                            });
                            if let Err(error) = persist_directory_manager_state(&manager) {
                                cleanup_local_directory(&staging_root);
                                set_get_failed(&manager, transfer_id, error.to_string());
                                return;
                            }
                            break;
                        }
                        ControllerFileTransferPhase::Failed => {
                            cleanup_local_directory(&staging_root);
                            set_get_failed(
                                &manager,
                                transfer_id,
                                info.error.unwrap_or_else(|| "child file Get failed".into()),
                            );
                            return;
                        }
                        ControllerFileTransferPhase::Cancelled => {
                            cleanup_local_directory(&staging_root);
                            set_get_cancelled(&manager, transfer_id);
                            persist_directory_manager_state_or_warn(&manager, transfer_id);
                            return;
                        }
                        _ => {}
                    }
                }
                Ok(FileTransferInfo::Put(_)) => {
                    cleanup_local_directory(&staging_root);
                    set_get_failed(
                        &manager,
                        transfer_id,
                        "child transfer direction changed".into(),
                    );
                    return;
                }
                Err(error) => {
                    cleanup_local_directory(&staging_root);
                    set_get_failed(&manager, transfer_id, error.to_string());
                    return;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                changed = cancel.changed() => {
                    match changed {
                        Ok(()) if *cancel.borrow() => {
                            let _ = file_transfers.cancel(child.transfer_id);
                            cleanup_local_directory(&staging_root);
                            set_get_cancelled(&manager, transfer_id);
                            persist_directory_manager_state_or_warn(&manager, transfer_id);
                            return;
                        }
                        Ok(()) => {}
                        Err(_) => return,
                    }
                }
            }
        }
    }

    if *cancel.borrow() {
        cleanup_local_directory(&staging_root);
        set_get_cancelled(&manager, transfer_id);
        persist_directory_manager_state_or_warn(&manager, transfer_id);
        return;
    }
    update_get_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Finalizing;
        info.confirmed_file_bytes = info.total_file_bytes;
        info.completed_files = info.total_files;
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
    if let Err(error) = persist_directory_manager_state(&manager) {
        cleanup_local_directory(&staging_root);
        set_get_failed(&manager, transfer_id, error.to_string());
        return;
    }

    if !source_verified {
        match remote
            .directory_tree(
                device_id,
                DirectoryTreeRequest::FinalizeGet {
                    manifest: manifest.clone(),
                },
            )
            .await
        {
            Ok(DirectoryTreeReply::Verified {
                transfer_id: verified_transfer,
                device_root: verified_root,
            }) if verified_transfer == transfer_id && verified_root == manifest.device_root => {}
            Ok(DirectoryTreeReply::Error(error)) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(&manager, transfer_id, error.message);
                return;
            }
            Ok(reply) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(
                    &manager,
                    transfer_id,
                    format!("unexpected directory Get finalize reply: {reply:?}"),
                );
                return;
            }
            Err(error) => {
                cleanup_local_directory(&staging_root);
                set_get_failed(&manager, transfer_id, error.to_string());
                return;
            }
        }
        source_verified = true;
        let Some(staging_string) = staging_root.to_str().map(str::to_owned) else {
            cleanup_local_directory(&staging_root);
            set_get_failed(
                &manager,
                transfer_id,
                ControllerDirectoryTransferError::InvalidDestinationPath.to_string(),
            );
            return;
        };
        let Some(final_string) = final_root.to_str().map(str::to_owned) else {
            cleanup_local_directory(&staging_root);
            set_get_failed(
                &manager,
                transfer_id,
                ControllerDirectoryTransferError::InvalidDestinationPath.to_string(),
            );
            return;
        };
        if let Err(error) = set_durable_state(
            &manager,
            transfer_id,
            DurableControllerDirectoryState::Get {
                manifest: manifest.clone(),
                staging_controller_root: staging_string,
                final_controller_root: final_string,
                source_verified,
            },
        )
        .and_then(|()| persist_directory_manager_state(&manager))
        {
            cleanup_local_directory(&staging_root);
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
    }

    let manifest_for_commit = manifest.clone();
    let staging_for_commit = staging_root.clone();
    let final_for_commit = final_root.clone();
    let (final_path, durability_warning) = match tokio::task::spawn_blocking(move || {
        finalize_local_get_staging(&staging_for_commit, &final_for_commit, &manifest_for_commit)
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            set_get_failed(&manager, transfer_id, error.to_string());
            return;
        }
        Err(_) => {
            set_get_failed(
                &manager,
                transfer_id,
                ControllerDirectoryTransferError::WorkerFailed.to_string(),
            );
            return;
        }
    };
    update_get_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Completed;
        info.confirmed_file_bytes = info.total_file_bytes;
        info.completed_files = info.total_files;
        info.final_destination_path = Some(final_path);
        info.error = durability_warning;
    });
    persist_directory_manager_state_or_warn(&manager, transfer_id);
}

async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) -> DirectoryCancelWake {
    if *cancel.borrow() {
        return DirectoryCancelWake::Requested;
    }
    loop {
        match cancel.changed().await {
            Ok(()) if *cancel.borrow() => return DirectoryCancelWake::Requested,
            Ok(()) => {}
            Err(_) => return DirectoryCancelWake::OwnerDropped,
        }
    }
}

async fn cancel_remote_directory(
    remote: &RemoteHub,
    device_id: DeviceId,
    manifest: &DirectoryTreeManifest,
) {
    let cleanup = remote.directory_tree(
        device_id,
        DirectoryTreeRequest::CancelPut {
            manifest: manifest.clone(),
        },
    );
    let _ = tokio::time::timeout(Duration::from_secs(30), cleanup).await;
}

async fn recover_directory_cancel(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    remote: &RemoteHub,
    file_transfers: &ControllerFileTransferManager,
    transfer_id: TransferId,
    device_id: DeviceId,
    info: &DirectoryTransferInfo,
    durable_state: Option<&DurableControllerDirectoryState>,
) {
    let current_child = match info {
        DirectoryTransferInfo::Put(info) => info.current_file_transfer_id,
        DirectoryTransferInfo::Get(info) => info.current_file_transfer_id,
    };
    if let Some(child_id) = current_child {
        let _ = file_transfers.cancel(child_id);
    }
    match durable_state {
        Some(DurableControllerDirectoryState::Put { manifest, .. }) => {
            cancel_remote_directory(remote, device_id, manifest).await;
            set_cancelled(manager, transfer_id);
        }
        Some(DurableControllerDirectoryState::Get {
            staging_controller_root,
            ..
        }) => {
            cleanup_local_directory(Path::new(staging_controller_root));
            set_get_cancelled(manager, transfer_id);
        }
        None => match info {
            DirectoryTransferInfo::Put(_) => set_cancelled(manager, transfer_id),
            DirectoryTransferInfo::Get(_) => set_get_cancelled(manager, transfer_id),
        },
    }
}

fn current_put_info(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
) -> Result<DirectoryPutInfo, ControllerDirectoryTransferError> {
    let Some(manager) = manager.upgrade() else {
        return Err(ControllerDirectoryTransferError::NotFound(transfer_id));
    };
    let transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
    match transfers.get(&transfer_id) {
        Some(DirectoryTransferEntry {
            info: DirectoryTransferInfo::Put(info),
            ..
        }) => Ok(info.clone()),
        Some(_) => Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "directory transfer direction changed".into(),
        )),
        None => Err(ControllerDirectoryTransferError::NotFound(transfer_id)),
    }
}

fn current_get_info(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
) -> Result<DirectoryGetInfo, ControllerDirectoryTransferError> {
    let Some(manager) = manager.upgrade() else {
        return Err(ControllerDirectoryTransferError::NotFound(transfer_id));
    };
    let transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
    match transfers.get(&transfer_id) {
        Some(DirectoryTransferEntry {
            info: DirectoryTransferInfo::Get(info),
            ..
        }) => Ok(info.clone()),
        Some(_) => Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "directory transfer direction changed".into(),
        )),
        None => Err(ControllerDirectoryTransferError::NotFound(transfer_id)),
    }
}

fn update_info(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    update: impl FnOnce(&mut DirectoryPutInfo),
) {
    let Some(manager) = manager.upgrade() else {
        return;
    };
    if let Ok(mut transfers) = manager.transfers.lock()
        && let Some(entry) = transfers.get_mut(&transfer_id)
        && let DirectoryTransferInfo::Put(info) = &mut entry.info
    {
        update(info);
    }
}

fn set_failed(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    update_info(manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Failed;
        info.error = Some(error);
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
}

fn set_cancelled(manager: &Weak<ControllerDirectoryTransferManagerInner>, transfer_id: TransferId) {
    update_info(manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Cancelled;
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
}

fn update_get_info(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    update: impl FnOnce(&mut DirectoryGetInfo),
) {
    let Some(manager) = manager.upgrade() else {
        return;
    };
    if let Ok(mut transfers) = manager.transfers.lock()
        && let Some(entry) = transfers.get_mut(&transfer_id)
        && let DirectoryTransferInfo::Get(info) = &mut entry.info
    {
        update(info);
    }
}

fn set_get_failed(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    update_get_info(manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Failed;
        info.error = Some(error);
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
}

fn set_get_cancelled(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
) {
    update_get_info(manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Cancelled;
        info.current_relative_path = None;
        info.current_file_transfer_id = None;
    });
}

fn set_durable_state(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    state: DurableControllerDirectoryState,
) -> Result<(), ControllerDirectoryTransferError> {
    let Some(manager) = manager.upgrade() else {
        return Ok(());
    };
    let mut transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
    let entry = transfers
        .get_mut(&transfer_id)
        .ok_or(ControllerDirectoryTransferError::NotFound(transfer_id))?;
    entry.durable_state = Some(state);
    Ok(())
}

fn persist_directory_manager_state(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
) -> Result<(), ControllerDirectoryTransferError> {
    let Some(manager) = manager.upgrade() else {
        return Ok(());
    };
    let transfers = manager
        .transfers
        .lock()
        .map_err(|_| ControllerDirectoryTransferError::StatePoisoned)?;
    if let Some(store) = &manager.state_store {
        store.persist(&transfers)?;
    }
    Ok(())
}

fn record_directory_journal_warning(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
    error: String,
) {
    let Some(manager) = manager.upgrade() else {
        return;
    };
    let Ok(mut transfers) = manager.transfers.lock() else {
        return;
    };
    let Some(entry) = transfers.get_mut(&transfer_id) else {
        return;
    };
    let warning = format!("directory transfer state journal persistence failed: {error}");
    match &mut entry.info {
        DirectoryTransferInfo::Put(info) => info.error = Some(warning),
        DirectoryTransferInfo::Get(info) => info.error = Some(warning),
    }
}

fn persist_directory_manager_state_or_warn(
    manager: &Weak<ControllerDirectoryTransferManagerInner>,
    transfer_id: TransferId,
) {
    if let Err(error) = persist_directory_manager_state(manager) {
        record_directory_journal_warning(manager, transfer_id, error.to_string());
    }
}

fn prune_terminal(transfers: &mut BTreeMap<TransferId, DirectoryTransferEntry>) {
    while transfers.len() >= HARD_MAX_CONTROLLER_DIRECTORY_TRANSFERS {
        let terminal = transfers
            .iter()
            .find(|(_, entry)| entry.info.phase().terminal())
            .map(|(transfer_id, _)| *transfer_id);
        let Some(transfer_id) = terminal else {
            break;
        };
        transfers.remove(&transfer_id);
    }
}

fn join_relative(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

fn join_device_relative(root: &str, relative: &str) -> String {
    if root.ends_with('/') || root.ends_with('\\') {
        format!("{root}{relative}")
    } else {
        format!("{root}/{relative}")
    }
}

fn local_get_paths(
    destination_path: &Path,
    transfer_id: TransferId,
) -> Result<(PathBuf, PathBuf), ControllerDirectoryTransferError> {
    let Some(Component::Normal(name)) = destination_path.components().next_back() else {
        return Err(ControllerDirectoryTransferError::InvalidDestinationPath);
    };
    let Some(parent) = destination_path.parent() else {
        return Err(ControllerDirectoryTransferError::InvalidDestinationPath);
    };
    let parent = fs::canonicalize(parent)?;
    let final_root = parent.join(name);
    if final_root.to_str().is_none()
        || final_root.to_string_lossy().len() > crate::MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
    {
        return Err(ControllerDirectoryTransferError::InvalidDestinationPath);
    }
    match fs::symlink_metadata(&final_root) {
        Ok(_) => return Err(ControllerDirectoryTransferError::DestinationConflict),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let staging_root = parent.join(format!(".clew-dir-{transfer_id}.part"));
    if staging_root.to_str().is_none()
        || staging_root.to_string_lossy().len() > crate::MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
    {
        return Err(ControllerDirectoryTransferError::InvalidDestinationPath);
    }
    match fs::symlink_metadata(&staging_root) {
        Ok(_) => return Err(ControllerDirectoryTransferError::DestinationConflict),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok((staging_root, final_root))
}

fn ensure_local_get_staging(
    staging_root: &Path,
    final_root: &Path,
    manifest: &DirectoryTreeManifest,
    allow_create: bool,
    accept_existing: bool,
) -> Result<(), ControllerDirectoryTransferError> {
    match fs::symlink_metadata(final_root) {
        Ok(_) => return Err(ControllerDirectoryTransferError::DestinationConflict),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::symlink_metadata(staging_root) {
        Ok(_) if !accept_existing => {
            return Err(ControllerDirectoryTransferError::DestinationConflict);
        }
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "Controller directory staging root is not a safe directory".into(),
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_create => {
            fs::create_dir(staging_root)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "Controller directory staging root disappeared after progress was persisted".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    for entry in &manifest.entries {
        if entry.kind != DirectoryTreeEntryKind::Directory {
            continue;
        }
        let path = join_relative(staging_root, &entry.relative_path);
        match fs::symlink_metadata(&path) {
            Ok(_) if !accept_existing => {
                return Err(ControllerDirectoryTransferError::DestinationConflict);
            }
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                        format!(
                            "Controller directory staging parent is unsafe: {}",
                            entry.relative_path
                        ),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_create => {
                fs::create_dir(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    format!(
                        "Controller directory staging parent disappeared: {}",
                        entry.relative_path
                    ),
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    sync_local_parent(staging_root)?;
    Ok(())
}

#[cfg(test)]
fn prepare_local_get_staging(
    destination_path: &Path,
    transfer_id: TransferId,
    manifest: &DirectoryTreeManifest,
) -> Result<(PathBuf, PathBuf), ControllerDirectoryTransferError> {
    let (staging_root, final_root) = local_get_paths(destination_path, transfer_id)?;
    if let Err(error) = ensure_local_get_staging(&staging_root, &final_root, manifest, true, false)
    {
        cleanup_local_directory(&staging_root);
        return Err(error);
    }
    Ok((staging_root, final_root))
}

fn recover_local_get_commit(
    staging_root: &Path,
    final_root: &Path,
    manifest: &DirectoryTreeManifest,
) -> Result<Option<String>, ControllerDirectoryTransferError> {
    let staging_exists = match fs::symlink_metadata(staging_root) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "Controller directory staging root became unsafe".into(),
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let final_metadata = match fs::symlink_metadata(final_root) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if staging_exists {
        if final_metadata.is_some() {
            return Err(ControllerDirectoryTransferError::DestinationConflict);
        }
        return Ok(None);
    }
    let Some(metadata) = final_metadata else {
        return Ok(None);
    };
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(ControllerDirectoryTransferError::DestinationConflict);
    }
    let scan = scan_directory_tree(final_root)
        .map_err(|error| ControllerDirectoryTransferError::LocalTree(error.to_string()))?;
    if scan.entries != manifest.entries || scan.total_file_bytes != manifest.total_file_bytes {
        return Err(ControllerDirectoryTransferError::LocalTreeMismatch);
    }
    final_root
        .to_str()
        .map(|path| Some(path.to_owned()))
        .ok_or(ControllerDirectoryTransferError::InvalidDestinationPath)
}

fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
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

fn finalize_local_get_staging(
    staging_root: &Path,
    final_root: &Path,
    manifest: &DirectoryTreeManifest,
) -> Result<(String, Option<String>), ControllerDirectoryTransferError> {
    let scan = scan_directory_tree(staging_root)
        .map_err(|error| ControllerDirectoryTransferError::LocalTree(error.to_string()))?;
    if scan.entries != manifest.entries || scan.total_file_bytes != manifest.total_file_bytes {
        cleanup_local_directory(staging_root);
        return Err(ControllerDirectoryTransferError::LocalTreeMismatch);
    }
    match fs::symlink_metadata(final_root) {
        Ok(_) => {
            cleanup_local_directory(staging_root);
            return Err(ControllerDirectoryTransferError::DestinationConflict);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            cleanup_local_directory(staging_root);
            return Err(error.into());
        }
    }
    fs::rename(staging_root, final_root)?;
    let final_path = final_root
        .to_str()
        .map(str::to_owned)
        .ok_or(ControllerDirectoryTransferError::InvalidDestinationPath)?;
    let durability_warning = sync_local_parent(final_root)
        .err()
        .map(|error| format!("directory committed but parent durability sync failed: {error}"));
    Ok((final_path, durability_warning))
}

fn cleanup_local_directory(staging_root: &Path) {
    match fs::symlink_metadata(staging_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => return,
        Ok(_) => {}
    }
    if scan_directory_tree(staging_root).is_ok() {
        let _ = fs::remove_dir_all(staging_root);
        let _ = sync_local_parent(staging_root);
    }
}

fn sync_local_parent(_path: &Path) -> Result<(), ControllerDirectoryTransferError> {
    #[cfg(unix)]
    {
        if let Some(parent) = _path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

fn validate_get_start_inputs(
    device_root: &str,
    destination_path: &str,
    chunk_size: u32,
) -> Result<(), ControllerDirectoryTransferError> {
    if device_root.trim().is_empty()
        || device_root.len() > clew_transport::MAX_FILE_RESUME_PATH_BYTES
        || device_root.contains('\0')
    {
        return Err(ControllerDirectoryTransferError::InvalidDeviceRoot);
    }
    if destination_path.trim().is_empty()
        || destination_path.len() > crate::MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
        || destination_path.contains('\0')
        || !Path::new(destination_path).is_absolute()
    {
        return Err(ControllerDirectoryTransferError::InvalidDestinationPath);
    }
    if chunk_size < clew_transport::MIN_FILE_CHUNK_BYTES
        || chunk_size > clew_transport::MAX_FILE_CHUNK_BYTES
        || !chunk_size.is_power_of_two()
    {
        return Err(ControllerDirectoryTransferError::InvalidChunkSize(
            chunk_size,
        ));
    }
    Ok(())
}

fn validate_start_inputs(
    source_path: &str,
    device_root: &str,
    chunk_size: u32,
) -> Result<(), ControllerDirectoryTransferError> {
    if source_path.trim().is_empty()
        || source_path.len() > crate::MAX_CONTROLLER_FILE_SOURCE_PATH_BYTES
        || source_path.contains('\0')
        || !Path::new(source_path).is_absolute()
    {
        return Err(ControllerDirectoryTransferError::InvalidSourcePath);
    }
    if device_root.trim().is_empty()
        || device_root.len() > clew_transport::MAX_FILE_RESUME_PATH_BYTES
        || device_root.contains('\0')
    {
        return Err(ControllerDirectoryTransferError::InvalidDeviceRoot);
    }
    if chunk_size < clew_transport::MIN_FILE_CHUNK_BYTES
        || chunk_size > clew_transport::MAX_FILE_CHUNK_BYTES
        || !chunk_size.is_power_of_two()
    {
        return Err(ControllerDirectoryTransferError::InvalidChunkSize(
            chunk_size,
        ));
    }
    Ok(())
}

fn validate_durable_directory_record(
    record: &DurableControllerDirectoryTransfer,
    controller_id: ControllerId,
) -> Result<(), ControllerDirectoryTransferError> {
    let (
        transfer_id,
        device_id,
        phase,
        total_file_bytes,
        confirmed_file_bytes,
        total_files,
        completed_files,
        current_relative_path,
        current_child,
    ) = match &record.info {
        DirectoryTransferInfo::Put(info) => {
            validate_start_inputs(&info.source_path, &info.device_root, info.chunk_size)?;
            (
                info.transfer_id,
                info.device_id,
                info.phase,
                info.total_file_bytes,
                info.confirmed_file_bytes,
                info.total_files,
                info.completed_files,
                info.current_relative_path.as_deref(),
                info.current_file_transfer_id,
            )
        }
        DirectoryTransferInfo::Get(info) => {
            validate_get_start_inputs(&info.device_root, &info.destination_path, info.chunk_size)?;
            (
                info.transfer_id,
                info.device_id,
                info.phase,
                info.total_file_bytes,
                info.confirmed_file_bytes,
                info.total_files,
                info.completed_files,
                info.current_relative_path.as_deref(),
                info.current_file_transfer_id,
            )
        }
    };
    if current_relative_path.is_some() != current_child.is_some() {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "directory current relative path and child TransferId must appear together".into(),
        ));
    }
    let Some(state) = &record.state else {
        if total_file_bytes != 0
            || confirmed_file_bytes != 0
            || total_files != 0
            || completed_files != 0
            || current_relative_path.is_some()
            || !matches!(
                phase,
                ControllerDirectoryTransferPhase::Preparing
                    | ControllerDirectoryTransferPhase::WaitingForReconnect
                    | ControllerDirectoryTransferPhase::Cancelling
                    | ControllerDirectoryTransferPhase::Failed
                    | ControllerDirectoryTransferPhase::Cancelled
            )
        {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "directory transfer without durable manifest carries peer-visible progress".into(),
            ));
        }
        return Ok(());
    };

    let manifest = match state {
        DurableControllerDirectoryState::Put {
            manifest,
            staging_device_root,
        } => {
            if !matches!(record.info, DirectoryTransferInfo::Put(_)) {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "directory durable Put state is attached to Get info".into(),
                ));
            }
            if let Some(staging) = staging_device_root
                && (staging.trim().is_empty()
                    || staging.len() > clew_transport::MAX_FILE_RESUME_PATH_BYTES
                    || staging.contains('\0'))
            {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "persisted Host directory staging root is invalid".into(),
                ));
            }
            manifest
        }
        DurableControllerDirectoryState::Get {
            manifest,
            staging_controller_root,
            final_controller_root,
            source_verified,
        } => {
            if !matches!(record.info, DirectoryTransferInfo::Get(_)) {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "directory durable Get state is attached to Put info".into(),
                ));
            }
            for path in [staging_controller_root, final_controller_root] {
                if path.trim().is_empty()
                    || path.len() > crate::MAX_CONTROLLER_FILE_DESTINATION_PATH_BYTES
                    || path.contains('\0')
                    || !Path::new(path).is_absolute()
                {
                    return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                        "persisted Controller directory path is invalid".into(),
                    ));
                }
            }
            let expected_staging = Path::new(staging_controller_root)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == format!(".clew-dir-{transfer_id}.part"))
                .unwrap_or(false);
            if !expected_staging
                || Path::new(staging_controller_root).parent()
                    != Path::new(final_controller_root).parent()
            {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "persisted Controller directory staging binding is inconsistent".into(),
                ));
            }
            if *source_verified
                && !matches!(
                    phase,
                    ControllerDirectoryTransferPhase::Finalizing
                        | ControllerDirectoryTransferPhase::Completed
                        | ControllerDirectoryTransferPhase::Failed
                )
            {
                return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                    "persisted Get source verification appears before directory finalization"
                        .into(),
                ));
            }
            manifest
        }
    };
    manifest.validate().map_err(|error| {
        ControllerDirectoryTransferError::InvalidPersistedState(format!(
            "persisted directory manifest is invalid: {error}"
        ))
    })?;
    if manifest.transfer_id != transfer_id
        || manifest.controller_id != controller_id
        || manifest.site_id != record.site_id
        || manifest.device_id != device_id
    {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "persisted directory manifest changed transfer scope".into(),
        ));
    }
    match (&record.info, state) {
        (
            DirectoryTransferInfo::Put(info),
            DurableControllerDirectoryState::Put { manifest, .. },
        ) if manifest.direction == FileTransferDirection::ControllerToDevice
            && manifest.device_conflict_policy == Some(DirectoryConflictPolicy::FailIfExists)
            && manifest.device_root == info.device_root => {}
        (
            DirectoryTransferInfo::Get(info),
            DurableControllerDirectoryState::Get { manifest, .. },
        ) if manifest.direction == FileTransferDirection::DeviceToController
            && manifest.device_conflict_policy.is_none()
            && manifest.device_root == info.device_root => {}
        _ => {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "persisted directory manifest direction/root does not match public info".into(),
            ));
        }
    }
    let files = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
        .collect::<Vec<_>>();
    if total_file_bytes != manifest.total_file_bytes
        || total_files != files.len() as u32
        || completed_files > total_files
        || confirmed_file_bytes > total_file_bytes
    {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "persisted directory aggregate progress is inconsistent with manifest".into(),
        ));
    }
    let completed_prefix_bytes: u64 = files
        .iter()
        .take(completed_files as usize)
        .map(|entry| entry.size)
        .sum();
    if confirmed_file_bytes < completed_prefix_bytes {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "persisted directory confirmed bytes regress behind completed file prefix".into(),
        ));
    }
    if let Some(relative) = current_relative_path {
        let Some(current) = files.get(completed_files as usize) else {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "persisted current child exists after all manifest files completed".into(),
            ));
        };
        if current.relative_path != relative
            || confirmed_file_bytes > completed_prefix_bytes.saturating_add(current.size)
        {
            return Err(ControllerDirectoryTransferError::InvalidPersistedState(
                "persisted current directory child is not the next manifest file".into(),
            ));
        }
    } else if confirmed_file_bytes != completed_prefix_bytes && !phase.terminal() {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "persisted directory bytes extend beyond completed prefix without a current child"
                .into(),
        ));
    }
    if phase == ControllerDirectoryTransferPhase::Completed
        && (completed_files != total_files
            || confirmed_file_bytes != total_file_bytes
            || current_child.is_some())
    {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "Completed directory transfer must cover the entire manifest".into(),
        ));
    }
    if matches!(phase, ControllerDirectoryTransferPhase::Finalizing)
        && completed_files != total_files
    {
        return Err(ControllerDirectoryTransferError::InvalidPersistedState(
            "directory Finalizing state requires every manifest file completed".into(),
        ));
    }
    Ok(())
}

enum ControllerDirectoryTransferSlotRead {
    Missing,
    Valid(ControllerDirectoryTransferSnapshot),
    Invalid(ControllerDirectoryTransferError),
}

fn read_controller_directory_transfer_slot(path: &Path) -> ControllerDirectoryTransferSlotRead {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ControllerDirectoryTransferSlotRead::Missing;
        }
        Err(error) => return ControllerDirectoryTransferSlotRead::Invalid(error.into()),
    };
    let mut encoded = Vec::new();
    if let Err(error) = Read::by_ref(&mut file)
        .take((MAX_STATE_DOCUMENT_SIZE + 1) as u64)
        .read_to_end(&mut encoded)
    {
        return ControllerDirectoryTransferSlotRead::Invalid(error.into());
    }
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return ControllerDirectoryTransferSlotRead::Invalid(
            ControllerDirectoryTransferError::InvalidPersistedState(
                "controller directory transfer journal exceeds the state document hard bound"
                    .into(),
            ),
        );
    }
    match decode_state_json(&encoded) {
        Ok(snapshot) => ControllerDirectoryTransferSlotRead::Valid(snapshot),
        Err(error) => ControllerDirectoryTransferSlotRead::Invalid(error.into()),
    }
}

fn write_controller_directory_transfer_slot(
    path: &Path,
    snapshot: &ControllerDirectoryTransferSnapshot,
) -> Result<(), ControllerDirectoryTransferError> {
    let parent = path.parent().ok_or_else(|| {
        ControllerDirectoryTransferError::InvalidPersistedState(
            "controller directory transfer journal path has no parent".into(),
        )
    })?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
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
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&encoded)?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ControllerDirectoryTransferError {
    #[error("Controller directory transfer state is unavailable")]
    StatePoisoned,
    #[error("Controller directory transfer state belongs to a different Controller")]
    ControllerMismatch,
    #[error("Controller directory transfer persisted state is invalid: {0}")]
    InvalidPersistedState(String),
    #[error("Controller directory transfer state has conflicting generation {0}")]
    StateGenerationConflict(u64),
    #[error("Controller directory transfer state generation overflow")]
    StateGenerationOverflow,
    #[error("Controller directory transfer capacity is exhausted")]
    Capacity,
    #[error("directory source path is invalid")]
    InvalidSourcePath,
    #[error("device directory root is invalid")]
    InvalidDeviceRoot,
    #[error("Controller directory destination path is invalid, relative, or too long")]
    InvalidDestinationPath,
    #[error("Controller directory destination or private staging path already exists")]
    DestinationConflict,
    #[error("file chunk size is invalid: {0}")]
    InvalidChunkSize(u32),
    #[error("Host returned an invalid directory manifest: {0}")]
    InvalidHostManifest(String),
    #[error("Controller directory staging tree does not match the device manifest")]
    LocalTreeMismatch,
    #[error("Controller directory staging scan failed: {0}")]
    LocalTree(String),
    #[error("Controller directory transfer blocking worker failed")]
    WorkerFailed,
    #[error("directory transfer was not found: {0}")]
    NotFound(TransferId),
    #[error(transparent)]
    StateCodec(#[from] StateCodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    FileTransfer(#[from] ControllerFileTransferError),
    #[error(transparent)]
    Remote(#[from] RemoteHubError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_transport::{DirectoryTreeEntry, file_sha256_hex};
    use tempfile::tempdir;

    fn local_get_manifest(
        transfer_id: TransferId,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> DirectoryTreeManifest {
        DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/source",
            vec![
                DirectoryTreeEntry::file("a.txt", 5, file_sha256_hex(b"alpha")).unwrap(),
                DirectoryTreeEntry::directory("empty").unwrap(),
                DirectoryTreeEntry::directory("nested").unwrap(),
                DirectoryTreeEntry::file("nested/b.txt", 4, file_sha256_hex(b"beta")).unwrap(),
            ],
            None,
        )
        .unwrap()
    }

    fn durable_directory_put_record(
        root: &Path,
        controller_id: ControllerId,
    ) -> (DurableControllerDirectoryTransfer, TransferId, TransferId) {
        let source = root.join("source-tree");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"alpha").unwrap();
        let transfer_id = TransferId::new();
        let child_id = TransferId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let manifest = DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::ControllerToDevice,
            "/device/target-tree",
            vec![DirectoryTreeEntry::file("a.txt", 5, file_sha256_hex(b"alpha")).unwrap()],
            Some(DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();
        (
            DurableControllerDirectoryTransfer {
                site_id,
                info: DirectoryTransferInfo::Put(DirectoryPutInfo {
                    transfer_id,
                    device_id,
                    source_path: source.to_string_lossy().into_owned(),
                    device_root: manifest.device_root.clone(),
                    phase: ControllerDirectoryTransferPhase::Running,
                    chunk_size: clew_transport::MIN_FILE_CHUNK_BYTES,
                    total_file_bytes: 5,
                    confirmed_file_bytes: 0,
                    total_files: 1,
                    completed_files: 0,
                    current_relative_path: Some("a.txt".into()),
                    current_file_transfer_id: Some(child_id),
                    final_device_root: None,
                    error: None,
                }),
                state: Some(DurableControllerDirectoryState::Put {
                    manifest,
                    staging_device_root: Some(format!(
                        "/device/.clew-directory-{transfer_id}.part"
                    )),
                }),
            },
            transfer_id,
            child_id,
        )
    }

    #[test]
    fn controller_directory_journal_recovers_previous_slot_and_keeps_reserved_child() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("state"));
        let controller_id = ControllerId::new();
        let (record, transfer_id, child_id) =
            durable_directory_put_record(temp.path(), controller_id);
        let (store, initial) =
            ControllerDirectoryTransferStateStore::load(layout.clone(), controller_id).unwrap();
        assert_eq!(initial.generation, 0);
        assert!(initial.transfers.is_empty());
        validate_durable_directory_record(&record, controller_id).unwrap();

        let DirectoryTransferInfo::Put(info) = record.info.clone() else {
            unreachable!();
        };
        let (cancel, _) = watch::channel(false);
        let mut transfers = BTreeMap::new();
        transfers.insert(
            transfer_id,
            DirectoryTransferEntry {
                info: DirectoryTransferInfo::Put(info),
                site_id: record.site_id,
                durable_state: record.state.clone(),
                cancel,
            },
        );
        store.persist(&transfers).unwrap();
        if let Some(entry) = transfers.get_mut(&transfer_id) {
            entry
                .info
                .set_phase(ControllerDirectoryTransferPhase::WaitingForReconnect);
        }
        store.persist(&transfers).unwrap();

        assert!(matches!(
            ControllerDirectoryTransferStateStore::load(layout.clone(), ControllerId::new()),
            Err(ControllerDirectoryTransferError::ControllerMismatch)
        ));
        fs::write(
            layout.controller_directory_transfer_slot_a_path(),
            b"{broken",
        )
        .unwrap();
        let (_reloaded, recovered) =
            ControllerDirectoryTransferStateStore::load(layout.clone(), controller_id).unwrap();
        assert_eq!(recovered.generation, 1);
        assert_eq!(recovered.transfers.len(), 1);
        assert_eq!(recovered.transfers[0].info.transfer_id(), transfer_id);
        let DirectoryTransferInfo::Put(recovered_info) = &recovered.transfers[0].info else {
            unreachable!();
        };
        assert_eq!(recovered_info.current_file_transfer_id, Some(child_id));
        assert_eq!(
            recovered_info.current_relative_path.as_deref(),
            Some("a.txt")
        );

        let duplicate = ControllerDirectoryTransferSnapshot {
            controller_id,
            generation: 9,
            transfers: vec![record.clone(), record],
        };
        assert!(matches!(
            duplicate.validate(controller_id),
            Err(ControllerDirectoryTransferError::InvalidPersistedState(_))
        ));
    }

    #[test]
    fn directory_journal_rejects_progress_without_manifest_or_mismatched_child_prefix() {
        let temp = tempdir().unwrap();
        let controller_id = ControllerId::new();
        let (mut record, _, _) = durable_directory_put_record(temp.path(), controller_id);
        record.state = None;
        assert!(matches!(
            validate_durable_directory_record(&record, controller_id),
            Err(ControllerDirectoryTransferError::InvalidPersistedState(_))
        ));

        let (mut record, _, _) = durable_directory_put_record(temp.path(), controller_id);
        let DirectoryTransferInfo::Put(info) = &mut record.info else {
            unreachable!();
        };
        info.current_relative_path = Some("wrong.txt".into());
        assert!(matches!(
            validate_durable_directory_record(&record, controller_id),
            Err(ControllerDirectoryTransferError::InvalidPersistedState(_))
        ));
    }

    #[test]
    fn local_directory_get_staging_is_manifest_bounded_and_atomically_committed() {
        let temp = tempdir().unwrap();
        let transfer_id = TransferId::new();
        let manifest = local_get_manifest(
            transfer_id,
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
        );
        let destination = temp.path().join("downloaded-tree");
        let (staging, final_root) =
            prepare_local_get_staging(&destination, transfer_id, &manifest).unwrap();
        assert_eq!(
            final_root,
            fs::canonicalize(temp.path())
                .unwrap()
                .join("downloaded-tree")
        );
        assert!(staging.join("empty").is_dir());
        assert!(staging.join("nested").is_dir());
        fs::write(staging.join("a.txt"), b"alpha").unwrap();
        fs::write(staging.join("nested/b.txt"), b"beta").unwrap();

        let (committed, warning) =
            finalize_local_get_staging(&staging, &final_root, &manifest).unwrap();
        assert_eq!(PathBuf::from(committed), final_root);
        assert!(warning.is_none());
        assert!(!staging.exists());
        let scan = scan_directory_tree(&destination).unwrap();
        assert_eq!(scan.entries, manifest.entries);
        assert_eq!(scan.total_file_bytes, manifest.total_file_bytes);
    }

    #[test]
    fn fresh_directory_get_staging_rejects_raced_preexisting_private_root() {
        let temp = tempdir().unwrap();
        let transfer_id = TransferId::new();
        let manifest = local_get_manifest(
            transfer_id,
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
        );
        let destination = temp.path().join("downloaded-tree");
        let (staging, final_root) = local_get_paths(&destination, transfer_id).unwrap();
        fs::create_dir(&staging).unwrap();
        assert!(matches!(
            ensure_local_get_staging(&staging, &final_root, &manifest, true, false),
            Err(ControllerDirectoryTransferError::DestinationConflict)
        ));
        fs::remove_dir(&staging).unwrap();
    }

    #[test]
    fn finalizing_directory_get_recovers_atomic_rename_before_completed_journal() {
        let temp = tempdir().unwrap();
        let transfer_id = TransferId::new();
        let manifest = local_get_manifest(
            transfer_id,
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
        );
        let destination = temp.path().join("downloaded-tree");
        let (staging, final_root) =
            prepare_local_get_staging(&destination, transfer_id, &manifest).unwrap();
        fs::write(staging.join("a.txt"), b"alpha").unwrap();
        fs::write(staging.join("nested/b.txt"), b"beta").unwrap();
        finalize_local_get_staging(&staging, &final_root, &manifest).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            recover_local_get_commit(&staging, &final_root, &manifest).unwrap(),
            Some(final_root.to_string_lossy().into_owned())
        );
        fs::write(final_root.join("a.txt"), b"wrong").unwrap();
        assert!(matches!(
            recover_local_get_commit(&staging, &final_root, &manifest),
            Err(ControllerDirectoryTransferError::LocalTreeMismatch)
        ));
    }

    #[test]
    fn local_directory_get_staging_rejects_conflict_and_hash_mismatch() {
        let temp = tempdir().unwrap();
        let transfer_id = TransferId::new();
        let manifest = local_get_manifest(
            transfer_id,
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
        );
        let destination = temp.path().join("downloaded-tree");
        fs::create_dir(&destination).unwrap();
        assert!(matches!(
            prepare_local_get_staging(&destination, transfer_id, &manifest),
            Err(ControllerDirectoryTransferError::DestinationConflict)
        ));
        fs::remove_dir(&destination).unwrap();

        let (staging, final_root) =
            prepare_local_get_staging(&destination, transfer_id, &manifest).unwrap();
        fs::write(staging.join("a.txt"), b"wrong").unwrap();
        fs::write(staging.join("nested/b.txt"), b"beta").unwrap();
        assert!(matches!(
            finalize_local_get_staging(&staging, &final_root, &manifest),
            Err(ControllerDirectoryTransferError::LocalTreeMismatch)
        ));
        assert!(!staging.exists());
        assert!(!destination.exists());
    }
}
