use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use clew_core::{ControllerId, DeviceId, SiteId, TransferId};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerDirectoryTransferPhase {
    Preparing,
    Running,
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
}

#[derive(Debug)]
struct DirectoryTransferEntry {
    info: DirectoryTransferInfo,
    cancel: watch::Sender<bool>,
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
            }),
        }
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
                cancel,
            },
        );
        drop(transfers);
        let manager = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let file_transfers = self.inner.file_transfers.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            run_directory_put(
                manager,
                remote,
                file_transfers,
                controller_id,
                transfer_id,
                device_id,
                site_id,
                source_path,
                device_root,
                chunk_size,
                cancel_rx,
            )
            .await;
        });
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
                cancel,
            },
        );
        drop(transfers);
        let manager = Arc::downgrade(&self.inner);
        let remote = self.inner.remote.clone();
        let file_transfers = self.inner.file_transfers.clone();
        let controller_id = self.inner.controller_id;
        tokio::spawn(async move {
            run_directory_get(
                manager,
                remote,
                file_transfers,
                controller_id,
                transfer_id,
                device_id,
                site_id,
                device_root,
                destination_path,
                chunk_size,
                cancel_rx,
            )
            .await;
        });
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
            ControllerDirectoryTransferPhase::Preparing | ControllerDirectoryTransferPhase::Running
        ) {
            entry
                .info
                .set_phase(ControllerDirectoryTransferPhase::Cancelling);
            let _ = entry.cancel.send(true);
        }
        Ok(entry.info.clone())
    }
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
    mut cancel: watch::Receiver<bool>,
) {
    let source = PathBuf::from(&source_path);
    let scan = match tokio::task::spawn_blocking(move || scan_directory_tree(&source)).await {
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
        return;
    }
    let canonical_root = scan.canonical_root().to_path_buf();
    let entries = scan.entries.clone();
    let total_file_bytes = scan.total_file_bytes;
    let total_files = entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
        .count() as u32;
    let manifest = match scan.into_manifest(
        transfer_id,
        controller_id,
        site_id,
        device_id,
        FileTransferDirection::ControllerToDevice,
        device_root,
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

    let prepare = remote.directory_tree(
        device_id,
        DirectoryTreeRequest::PreparePut {
            manifest: manifest.clone(),
        },
    );
    tokio::pin!(prepare);
    let prepare_reply = tokio::select! {
        reply = &mut prepare => reply,
        _ = wait_for_cancel(&mut cancel) => {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_cancelled(&manager, transfer_id);
            return;
        }
    };
    let staging_root = match prepare_reply {
        Ok(DirectoryTreeReply::Prepared {
            staging_device_root,
            ..
        }) => staging_device_root,
        Ok(DirectoryTreeReply::Completed {
            final_device_root, ..
        }) => {
            update_info(&manager, transfer_id, |info| {
                info.phase = ControllerDirectoryTransferPhase::Completed;
                info.confirmed_file_bytes = info.total_file_bytes;
                info.completed_files = info.total_files;
                info.final_device_root = Some(final_device_root);
            });
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

    update_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Running;
    });

    let mut completed_bytes = 0_u64;

    for entry in entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
    {
        if *cancel.borrow() {
            cancel_remote_directory(&remote, device_id, &manifest).await;
            set_cancelled(&manager, transfer_id);
            return;
        }
        let local_path = join_relative(&canonical_root, &entry.relative_path);
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
        let child = match file_transfers.start_put(
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
        };
        update_info(&manager, transfer_id, |info| {
            info.current_relative_path = Some(entry.relative_path.clone());
            info.current_file_transfer_id = Some(child.transfer_id);
        });

        loop {
            if *cancel.borrow() {
                let _ = file_transfers.cancel(child.transfer_id);
                cancel_remote_directory(&remote, device_id, &manifest).await;
                set_cancelled(&manager, transfer_id);
                return;
            }
            match file_transfers.status(child.transfer_id) {
                Ok(FileTransferInfo::Put(info)) => {
                    update_info(&manager, transfer_id, |directory| {
                        directory.confirmed_file_bytes =
                            completed_bytes.saturating_add(info.confirmed_offset.min(entry.size));
                    });
                    match info.phase {
                        ControllerFileTransferPhase::Completed => {
                            completed_bytes = completed_bytes.saturating_add(entry.size);
                            update_info(&manager, transfer_id, |directory| {
                                directory.completed_files =
                                    directory.completed_files.saturating_add(1);
                                directory.confirmed_file_bytes = completed_bytes;
                                directory.current_relative_path = None;
                                directory.current_file_transfer_id = None;
                            });
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
                    if changed.is_err() || *cancel.borrow() {
                        let _ = file_transfers.cancel(child.transfer_id);
                        cancel_remote_directory(&remote, device_id, &manifest).await;
                        set_cancelled(&manager, transfer_id);
                        return;
                    }
                }
            }
        }
    }

    if *cancel.borrow() {
        cancel_remote_directory(&remote, device_id, &manifest).await;
        set_cancelled(&manager, transfer_id);
        return;
    }
    update_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Finalizing;
    });
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
        }) => update_info(&manager, transfer_id, |info| {
            info.phase = ControllerDirectoryTransferPhase::Completed;
            info.final_device_root = Some(final_device_root);
        }),
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
    let prepare = remote.directory_tree(
        device_id,
        DirectoryTreeRequest::PrepareGet {
            scope: scope.clone(),
        },
    );
    tokio::pin!(prepare);
    let prepare_reply = tokio::select! {
        reply = &mut prepare => reply,
        _ = wait_for_cancel(&mut cancel) => {
            set_get_cancelled(&manager, transfer_id);
            return;
        }
    };
    let manifest = match prepare_reply {
        Ok(DirectoryTreeReply::Manifest { manifest }) => manifest,
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
    };
    if manifest.validate().is_err()
        || manifest.transfer_id != transfer_id
        || manifest.controller_id != controller_id
        || manifest.site_id != site_id
        || manifest.device_id != device_id
        || manifest.direction != FileTransferDirection::DeviceToController
        || manifest.device_root != device_root
        || manifest.device_conflict_policy.is_some()
    {
        set_get_failed(
            &manager,
            transfer_id,
            "Host returned a directory manifest with changed transfer scope".into(),
        );
        return;
    }
    let entries = manifest.entries.clone();
    let total_files = entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
        .count() as u32;
    update_get_info(&manager, transfer_id, |info| {
        info.total_file_bytes = manifest.total_file_bytes;
        info.total_files = total_files;
    });

    let destination = PathBuf::from(&destination_path);
    let manifest_for_staging = manifest.clone();
    let staging = match tokio::task::spawn_blocking(move || {
        prepare_local_get_staging(&destination, transfer_id, &manifest_for_staging)
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
    let (staging_root, final_root) = staging;
    if *cancel.borrow() {
        cleanup_local_directory(&staging_root);
        set_get_cancelled(&manager, transfer_id);
        return;
    }
    update_get_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Running;
    });

    let mut completed_bytes = 0_u64;
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == DirectoryTreeEntryKind::File)
    {
        if *cancel.borrow() {
            cleanup_local_directory(&staging_root);
            set_get_cancelled(&manager, transfer_id);
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
        let child = match file_transfers.start_get(
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
        };
        update_get_info(&manager, transfer_id, |info| {
            info.current_relative_path = Some(entry.relative_path.clone());
            info.current_file_transfer_id = Some(child.transfer_id);
        });

        loop {
            if *cancel.borrow() {
                let _ = file_transfers.cancel(child.transfer_id);
                cleanup_local_directory(&staging_root);
                set_get_cancelled(&manager, transfer_id);
                return;
            }
            match file_transfers.status(child.transfer_id) {
                Ok(FileTransferInfo::Get(info)) => {
                    update_get_info(&manager, transfer_id, |directory| {
                        directory.confirmed_file_bytes =
                            completed_bytes.saturating_add(info.confirmed_offset.min(entry.size));
                    });
                    match info.phase {
                        ControllerFileTransferPhase::Completed => {
                            completed_bytes = completed_bytes.saturating_add(entry.size);
                            update_get_info(&manager, transfer_id, |directory| {
                                directory.completed_files =
                                    directory.completed_files.saturating_add(1);
                                directory.confirmed_file_bytes = completed_bytes;
                                directory.current_relative_path = None;
                                directory.current_file_transfer_id = None;
                            });
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
                    if changed.is_err() || *cancel.borrow() {
                        let _ = file_transfers.cancel(child.transfer_id);
                        cleanup_local_directory(&staging_root);
                        set_get_cancelled(&manager, transfer_id);
                        return;
                    }
                }
            }
        }
    }

    if *cancel.borrow() {
        cleanup_local_directory(&staging_root);
        set_get_cancelled(&manager, transfer_id);
        return;
    }
    update_get_info(&manager, transfer_id, |info| {
        info.phase = ControllerDirectoryTransferPhase::Finalizing;
    });
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

    let manifest_for_commit = manifest.clone();
    let (final_path, durability_warning) = match tokio::task::spawn_blocking(move || {
        finalize_local_get_staging(&staging_root, &final_root, &manifest_for_commit)
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
        // Once rename committed, a parent-directory sync failure must not be reported as an
        // uncommitted transfer. Preserve the durability warning while keeping phase truthful.
        info.error = durability_warning;
    });
}

async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    loop {
        match cancel.changed().await {
            Ok(()) if *cancel.borrow() => return,
            Ok(()) => {}
            Err(_) => return,
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

fn prepare_local_get_staging(
    destination_path: &Path,
    transfer_id: TransferId,
    manifest: &DirectoryTreeManifest,
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
    fs::create_dir(&staging_root)?;
    for entry in &manifest.entries {
        if entry.kind != DirectoryTreeEntryKind::Directory {
            continue;
        }
        if let Err(error) = fs::create_dir(join_relative(&staging_root, &entry.relative_path)) {
            cleanup_local_directory(&staging_root);
            return Err(error.into());
        }
    }
    if let Err(error) = sync_local_parent(&staging_root) {
        cleanup_local_directory(&staging_root);
        return Err(error);
    }
    Ok((staging_root, final_root))
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

#[derive(Debug, Error)]
pub enum ControllerDirectoryTransferError {
    #[error("Controller directory transfer state is unavailable")]
    StatePoisoned,
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
