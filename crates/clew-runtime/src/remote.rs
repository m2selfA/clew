use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{DeviceId, DeviceNameOrigin, DeviceRecord, RequestId, SiteId, TaskId};
use clew_host::normalize_hostname;
use clew_identity::{EnrollmentError, StoredControllerIdentity};
use clew_transport::{
    BootstrapErrorBody, BootstrapErrorCode, BootstrapMemberMode, BootstrapRequest,
    BootstrapResponse, ConnectorControlError, ConnectorLeaseError, ConnectorTunnelPurpose,
    ControllerSessionAuthority, DirectoryTreeReply, DirectoryTreeRequest, FileTransferReply,
    FileTransferRequest, FsMutationReply, FsMutationRequest, FsQueryReply, FsQueryRequest,
    HARD_MAX_SHELL_TASKS_PER_SESSION, InnerMessage, InnerSession, IrohProtocol, IrohStream,
    ReadReply, ReadRequest, SHELL_RECONNECT_GRACE_MS, SealedBootstrapContext, SealedBootstrapError,
    SealedBootstrapSession, ShellTaskErrorCode, ShellTaskReply, ShellTaskRequest,
    SignedConnectorLease, SiteDiscoveryTag, TcpForwardReply, TcpForwardRequest, is_rpc_progress,
    read_bootstrap, read_connector_open, unwrap_rpc_reply, wrap_rpc_request, write_bootstrap,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;

use crate::{ControlStoreError, ControllerControlStore};

pub const MAX_REMOTE_CONNECTIONS: usize = 128;
const REMOTE_COMMAND_CAPACITY: usize = 16;
const MAX_REMOTE_SHELL_TASK_PROJECTIONS: usize =
    MAX_REMOTE_CONNECTIONS * HARD_MAX_SHELL_TASKS_PER_SESSION;
const CONNECTOR_LEASE_TTL_MS: u64 = 5 * 60 * 1000;
const SEALED_ACTIVATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_CONTINUITY_GAP_MS: u64 = 15_000;

#[derive(Clone, Copy, Debug)]
struct SessionContinuity {
    last_peer_activity_unix_ms: u64,
}

impl SessionContinuity {
    fn new(now_unix_ms: u64) -> Self {
        Self {
            last_peer_activity_unix_ms: now_unix_ms,
        }
    }

    fn ensure_current(&self, now_unix_ms: u64) -> Result<(), RemoteHubError> {
        if now_unix_ms < self.last_peer_activity_unix_ms
            || now_unix_ms.saturating_sub(self.last_peer_activity_unix_ms)
                > SESSION_CONTINUITY_GAP_MS
        {
            return Err(RemoteHubError::SessionContinuityLost);
        }
        Ok(())
    }

    fn observe_peer_activity(&mut self, now_unix_ms: u64) -> Result<(), RemoteHubError> {
        self.ensure_current(now_unix_ms)?;
        self.last_peer_activity_unix_ms = now_unix_ms;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionTopology {
    Direct,
    Connector,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePathState {
    Direct,
    Relay,
    MixedOrUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionState {
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteSessionInfo {
    pub device_id: DeviceId,
    /// Monotonic only within the current Controller process instance.
    pub generation: u64,
    pub state: RemoteSessionState,
    pub topology: RemoteSessionTopology,
    pub path: RemotePathState,
    pub connected_unix_ms: u64,
    pub last_transition_unix_ms: u64,
    pub last_path_change_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub struct RemoteHub {
    inner: Arc<Mutex<RemoteHubState>>,
    session_changes: watch::Sender<u64>,
}

impl Default for RemoteHub {
    fn default() -> Self {
        let (session_changes, _) = watch::channel(0_u64);
        Self {
            inner: Arc::new(Mutex::new(RemoteHubState::default())),
            session_changes,
        }
    }
}

#[derive(Debug, Default)]
struct RemoteHubState {
    next_generation: u64,
    sessions: BTreeMap<DeviceId, RemoteSessionSlot>,
    session_info: BTreeMap<DeviceId, RemoteSessionInfo>,
    shell_tasks: BTreeMap<TaskId, RemoteShellTaskProjection>,
    pending_shell_starts: usize,
}

#[derive(Debug)]
struct RemoteSessionSlot {
    generation: u64,
    tx: mpsc::Sender<RemoteCommand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoteShellTaskProjection {
    device_id: DeviceId,
    generation: u64,
    reattach_deadline_unix_ms: Option<u64>,
}

struct ShellStartReservation {
    hub: RemoteHub,
    active: bool,
}

impl ShellStartReservation {
    fn new(hub: RemoteHub) -> Self {
        Self { hub, active: true }
    }

    fn release(&mut self) {
        if self.active {
            if let Ok(mut state) = self.hub.inner.lock() {
                state.pending_shell_starts = state.pending_shell_starts.saturating_sub(1);
            }
            self.active = false;
        }
    }
}

impl Drop for ShellStartReservation {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
enum RemoteCommand {
    Read {
        request_id: RequestId,
        request: ReadRequest,
        reply: oneshot::Sender<Result<ReadReply, RemoteHubError>>,
    },
    FsQuery {
        request_id: RequestId,
        request: FsQueryRequest,
        reply: oneshot::Sender<Result<FsQueryReply, RemoteHubError>>,
    },
    FsMutation {
        request_id: RequestId,
        request: FsMutationRequest,
        reply: oneshot::Sender<Result<FsMutationReply, RemoteHubError>>,
    },
    FileTransfer {
        request_id: RequestId,
        request: FileTransferRequest,
        reply: oneshot::Sender<Result<FileTransferReply, RemoteHubError>>,
    },
    DirectoryTree {
        request_id: RequestId,
        request: DirectoryTreeRequest,
        reply: oneshot::Sender<Result<DirectoryTreeReply, RemoteHubError>>,
    },
    TcpForward {
        request_id: RequestId,
        request: TcpForwardRequest,
        reply: oneshot::Sender<Result<TcpForwardReply, RemoteHubError>>,
    },
    ShellTask {
        request_id: RequestId,
        request: ShellTaskRequest,
        reply: oneshot::Sender<Result<ShellTaskReply, RemoteHubError>>,
    },
    Stop,
}

impl RemoteHub {
    #[must_use]
    pub fn is_online(&self, device_id: DeviceId) -> bool {
        self.inner
            .lock()
            .expect("remote hub mutex poisoned")
            .sessions
            .contains_key(&device_id)
    }

    #[must_use]
    pub fn online_device_ids(&self) -> Vec<DeviceId> {
        self.inner
            .lock()
            .expect("remote hub mutex poisoned")
            .sessions
            .keys()
            .copied()
            .collect()
    }

    pub fn session_info(
        &self,
        device_id: DeviceId,
    ) -> Result<Option<RemoteSessionInfo>, RemoteHubError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .session_info
            .get(&device_id)
            .cloned())
    }

    fn update_path(
        &self,
        device_id: DeviceId,
        generation: u64,
        path: RemotePathState,
        now_unix_ms: u64,
    ) {
        if let Ok(mut state) = self.inner.lock()
            && state
                .sessions
                .get(&device_id)
                .is_some_and(|slot| slot.generation == generation)
            && let Some(info) = state.session_info.get_mut(&device_id)
            && info.generation == generation
            && info.path != path
        {
            info.path = path;
            info.last_path_change_unix_ms = now_unix_ms;
        }
    }

    fn signal_session_change(&self) {
        self.session_changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    async fn replay_session_after(
        &self,
        device_id: DeviceId,
        previous_generation: Option<u64>,
    ) -> Result<(u64, mpsc::Sender<RemoteCommand>), RemoteHubError> {
        let mut changes = self.session_changes.subscribe();
        loop {
            let current = {
                let state = self
                    .inner
                    .lock()
                    .map_err(|_| RemoteHubError::StatePoisoned)?;
                state
                    .sessions
                    .get(&device_id)
                    .map(|slot| (slot.generation, slot.tx.clone()))
            };
            if let Some((generation, tx)) = current
                && previous_generation != Some(generation)
            {
                return Ok((generation, tx));
            }
            changes
                .changed()
                .await
                .map_err(|_| RemoteHubError::Offline(device_id))?;
        }
    }

    pub async fn read(
        &self,
        device_id: DeviceId,
        request: ReadRequest,
    ) -> Result<ReadReply, RemoteHubError> {
        let request_id = RequestId::new();
        let mut previous_generation = None;
        loop {
            let (generation, tx) = self
                .replay_session_after(device_id, previous_generation)
                .await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(RemoteCommand::Read {
                    request_id,
                    request: request.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                previous_generation = Some(generation);
                continue;
            }
            match reply_rx.await {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(_)) | Err(_) => previous_generation = Some(generation),
            }
        }
    }

    pub async fn fs_query(
        &self,
        device_id: DeviceId,
        request: FsQueryRequest,
    ) -> Result<FsQueryReply, RemoteHubError> {
        let request_id = RequestId::new();
        let mut previous_generation = None;
        loop {
            let (generation, tx) = self
                .replay_session_after(device_id, previous_generation)
                .await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(RemoteCommand::FsQuery {
                    request_id,
                    request: request.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                previous_generation = Some(generation);
                continue;
            }
            match reply_rx.await {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(_)) | Err(_) => previous_generation = Some(generation),
            }
        }
    }

    pub async fn fs_mutation(
        &self,
        device_id: DeviceId,
        request: FsMutationRequest,
    ) -> Result<FsMutationReply, RemoteHubError> {
        let request_id = RequestId::new();
        let mut previous_generation = None;
        loop {
            let (generation, tx) = self
                .replay_session_after(device_id, previous_generation)
                .await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(RemoteCommand::FsMutation {
                    request_id,
                    request: request.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                previous_generation = Some(generation);
                continue;
            }
            match reply_rx.await {
                Ok(Ok(FsMutationReply::Error(error)))
                    if error.code == clew_transport::FsMutationErrorCode::Timeout =>
                {
                    previous_generation = None;
                }
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(_)) | Err(_) => previous_generation = Some(generation),
            }
        }
    }

    pub async fn directory_tree(
        &self,
        device_id: DeviceId,
        request: DirectoryTreeRequest,
    ) -> Result<DirectoryTreeReply, RemoteHubError> {
        request.validate()?;
        let request_id = RequestId::new();
        let mut previous_generation = None;
        loop {
            let (generation, tx) = self
                .replay_session_after(device_id, previous_generation)
                .await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx
                .send(RemoteCommand::DirectoryTree {
                    request_id,
                    request: request.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                previous_generation = Some(generation);
                continue;
            }
            match reply_rx.await {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(_)) | Err(_) => previous_generation = Some(generation),
            }
        }
    }

    /// Executes one file-transfer RPC on the current/new session without generic replay.
    ///
    /// When `previous_generation` is Some, this waits for a different authenticated generation.
    /// The transfer owner must reconcile Host Status/confirmed_offset before deciding what to send
    /// next; generic RequestId replay is deliberately not used for file chunks.
    pub async fn file_transfer_attempt(
        &self,
        device_id: DeviceId,
        previous_generation: Option<u64>,
        request: FileTransferRequest,
    ) -> Result<(u64, Result<FileTransferReply, RemoteHubError>), RemoteHubError> {
        request.validate()?;
        let (generation, tx) = self
            .replay_session_after(device_id, previous_generation)
            .await?;
        Ok((
            generation,
            send_file_transfer_command(&tx, device_id, request).await,
        ))
    }

    pub async fn file_transfer_on_generation(
        &self,
        device_id: DeviceId,
        expected_generation: u64,
        request: FileTransferRequest,
    ) -> Result<Result<FileTransferReply, RemoteHubError>, RemoteHubError> {
        request.validate()?;
        let tx = {
            let state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
            let Some(slot) = state.sessions.get(&device_id) else {
                return Ok(Err(RemoteHubError::Offline(device_id)));
            };
            if slot.generation != expected_generation {
                return Ok(Err(RemoteHubError::SessionGenerationChanged {
                    expected: expected_generation,
                    actual: slot.generation,
                }));
            }
            slot.tx.clone()
        };
        Ok(send_file_transfer_command(&tx, device_id, request).await)
    }

    pub async fn tcp_forward_open(
        &self,
        device_id: DeviceId,
        request: TcpForwardRequest,
    ) -> Result<(u64, TcpForwardReply), RemoteHubError> {
        if !matches!(request, TcpForwardRequest::Open { .. }) {
            return Err(RemoteHubError::InvalidTcpForwardRequestShape);
        }
        request.validate()?;
        let (generation, tx) = self.current_session(device_id)?;
        let reply = send_tcp_forward_command(&tx, device_id, request.clone()).await?;
        validate_tcp_forward_reply_correlation(&request, &reply)?;
        self.ensure_generation(device_id, generation)?;
        Ok((generation, reply))
    }

    pub async fn tcp_forward_on_generation(
        &self,
        device_id: DeviceId,
        generation: u64,
        request: TcpForwardRequest,
    ) -> Result<TcpForwardReply, RemoteHubError> {
        if matches!(request, TcpForwardRequest::Open { .. }) {
            return Err(RemoteHubError::InvalidTcpForwardRequestShape);
        }
        request.validate()?;
        let tx = self.session_sender_for_generation(device_id, generation)?;
        let reply = send_tcp_forward_command(&tx, device_id, request.clone()).await?;
        validate_tcp_forward_reply_correlation(&request, &reply)?;
        self.ensure_generation(device_id, generation)?;
        Ok(reply)
    }

    fn current_session(
        &self,
        device_id: DeviceId,
    ) -> Result<(u64, mpsc::Sender<RemoteCommand>), RemoteHubError> {
        self.inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .map(|slot| (slot.generation, slot.tx.clone()))
            .ok_or(RemoteHubError::Offline(device_id))
    }

    fn session_sender_for_generation(
        &self,
        device_id: DeviceId,
        generation: u64,
    ) -> Result<mpsc::Sender<RemoteCommand>, RemoteHubError> {
        self.inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .filter(|slot| slot.generation == generation)
            .map(|slot| slot.tx.clone())
            .ok_or(RemoteHubError::TcpForwardSessionChanged(device_id))
    }

    fn ensure_generation(
        &self,
        device_id: DeviceId,
        generation: u64,
    ) -> Result<(), RemoteHubError> {
        if self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .is_some_and(|slot| slot.generation == generation)
        {
            Ok(())
        } else {
            Err(RemoteHubError::TcpForwardSessionChanged(device_id))
        }
    }
    pub async fn shell_start(
        &self,
        device_id: DeviceId,
        request: ShellTaskRequest,
    ) -> Result<ShellTaskReply, RemoteHubError> {
        if !matches!(request, ShellTaskRequest::Start { .. }) {
            return Err(RemoteHubError::InvalidShellProjectionRequest);
        }
        request.validate()?;
        let (generation, tx) = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
            prune_expired_shell_projections(&mut state, unix_ms().ok().unwrap_or(u64::MAX));
            let Some(slot) = state.sessions.get(&device_id) else {
                return Err(RemoteHubError::Offline(device_id));
            };
            if state
                .shell_tasks
                .len()
                .saturating_add(state.pending_shell_starts)
                >= MAX_REMOTE_SHELL_TASK_PROJECTIONS
            {
                return Err(RemoteHubError::ShellProjectionCapacity);
            }
            let generation = slot.generation;
            let tx = slot.tx.clone();
            state.pending_shell_starts += 1;
            (generation, tx)
        };
        let reservation = ShellStartReservation::new(self.clone());
        let request_id = RequestId::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::ShellTask {
            request_id,
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;

        let hub = self.clone();
        let (completion_tx, completion_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut reservation = reservation;
            let reply = reply_rx
                .await
                .map_err(|_| RemoteHubError::Offline(device_id))
                .and_then(|reply| reply);
            reservation.release();

            let mut known_task_id = None;
            let completion = match reply {
                Ok(ShellTaskReply::Started { task_id }) => {
                    known_task_id = Some(task_id);
                    let projection_ready = {
                        let mut state = hub.inner.lock().map_err(|_| RemoteHubError::StatePoisoned);
                        match &mut state {
                            Ok(state)
                                if state
                                    .sessions
                                    .get(&device_id)
                                    .is_some_and(|slot| slot.generation == generation) =>
                            {
                                if state.shell_tasks.contains_key(&task_id) {
                                    Err(RemoteHubError::ShellTaskIdConflict(task_id))
                                } else {
                                    state.shell_tasks.insert(
                                        task_id,
                                        RemoteShellTaskProjection {
                                            device_id,
                                            generation,
                                            reattach_deadline_unix_ms: None,
                                        },
                                    );
                                    Ok(())
                                }
                            }
                            Ok(_) => Err(RemoteHubError::Offline(device_id)),
                            Err(_) => Err(RemoteHubError::StatePoisoned),
                        }
                    };
                    match projection_ready {
                        Err(error) => Err(error),
                        Ok(()) => {
                            match hub.shell_task(ShellTaskRequest::Status { task_id }).await {
                                Ok((resolved_device, ShellTaskReply::Status(_)))
                                    if resolved_device == device_id =>
                                {
                                    Ok(ShellTaskReply::Started { task_id })
                                }
                                Ok((_, ShellTaskReply::Error(error))) => {
                                    Ok(ShellTaskReply::Error(error))
                                }
                                Ok(_) => Err(RemoteHubError::ShellReplyMismatch),
                                Err(error) => Err(error),
                            }
                        }
                    }
                }
                Ok(ShellTaskReply::Error(error)) => Ok(ShellTaskReply::Error(error)),
                Ok(_) => Err(RemoteHubError::ShellReplyMismatch),
                Err(error) => Err(error),
            };

            let completed_start = matches!(&completion, Ok(ShellTaskReply::Started { .. }));
            let delivery_failed = completion_tx.send(completion).is_err();
            if let Some(task_id) = known_task_id
                && (!completed_start || delivery_failed)
            {
                let _ = hub.shell_task(ShellTaskRequest::Cancel { task_id }).await;
                if let Ok(mut state) = hub.inner.lock()
                    && state
                        .shell_tasks
                        .get(&task_id)
                        .is_some_and(|projection| projection.device_id == device_id)
                {
                    state.shell_tasks.remove(&task_id);
                }
            }
        });
        completion_rx
            .await
            .map_err(|_| RemoteHubError::Offline(device_id))?
    }

    pub fn shell_task_device(&self, task_id: TaskId) -> Result<DeviceId, RemoteHubError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?;
        prune_expired_shell_projections(&mut state, unix_ms().ok().unwrap_or(u64::MAX));
        state
            .shell_tasks
            .get(&task_id)
            .map(|projection| projection.device_id)
            .ok_or(RemoteHubError::UnknownShellTask(task_id))
    }

    pub async fn shell_task(
        &self,
        request: ShellTaskRequest,
    ) -> Result<(DeviceId, ShellTaskReply), RemoteHubError> {
        request.validate()?;
        let task_id =
            shell_request_task_id(&request).ok_or(RemoteHubError::InvalidShellProjectionRequest)?;
        let now = unix_ms().ok().unwrap_or(u64::MAX);
        let projection = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
            prune_expired_shell_projections(&mut state, now);
            state
                .shell_tasks
                .get(&task_id)
                .copied()
                .ok_or(RemoteHubError::UnknownShellTask(task_id))?
        };
        let device_id = projection.device_id;
        let wait_for_session = self.replay_session_after(device_id, None);
        let (generation, tx) = if let Some(deadline) = projection.reattach_deadline_unix_ms {
            if now > deadline {
                if let Ok(mut state) = self.inner.lock() {
                    state.shell_tasks.remove(&task_id);
                }
                return Err(RemoteHubError::UnknownShellTask(task_id));
            }
            match tokio::time::timeout(
                Duration::from_millis(deadline.saturating_sub(now)),
                wait_for_session,
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    if let Ok(mut state) = self.inner.lock() {
                        state.shell_tasks.remove(&task_id);
                    }
                    return Err(RemoteHubError::UnknownShellTask(task_id));
                }
            }
        } else {
            wait_for_session.await?
        };

        if generation != projection.generation {
            let proof_request = ShellTaskRequest::Status { task_id };
            let proof = send_shell_task_command(&tx, device_id, proof_request.clone()).await?;
            validate_shell_reply_correlation(&proof_request, &proof)?;
            match &proof {
                ShellTaskReply::Status(_) => {
                    let mut state = self
                        .inner
                        .lock()
                        .map_err(|_| RemoteHubError::StatePoisoned)?;
                    let session_matches = state
                        .sessions
                        .get(&device_id)
                        .is_some_and(|slot| slot.generation == generation);
                    let projection_matches = state
                        .shell_tasks
                        .get(&task_id)
                        .is_some_and(|current| current.device_id == device_id);
                    if !session_matches || !projection_matches {
                        return Err(RemoteHubError::Offline(device_id));
                    }
                    state.shell_tasks.insert(
                        task_id,
                        RemoteShellTaskProjection {
                            device_id,
                            generation,
                            reattach_deadline_unix_ms: None,
                        },
                    );
                    if matches!(request, ShellTaskRequest::Status { .. }) {
                        return Ok((device_id, proof));
                    }
                }
                ShellTaskReply::Error(error) if error.code == ShellTaskErrorCode::NotFound => {
                    if let Ok(mut state) = self.inner.lock() {
                        state.shell_tasks.remove(&task_id);
                    }
                    return Ok((device_id, proof));
                }
                ShellTaskReply::Error(_) => return Ok((device_id, proof)),
                _ => return Err(RemoteHubError::ShellReplyMismatch),
            }
        }

        let reply = send_shell_task_command(&tx, device_id, request.clone()).await?;
        validate_shell_reply_correlation(&request, &reply)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?;
        if !state
            .sessions
            .get(&device_id)
            .is_some_and(|slot| slot.generation == generation)
        {
            mark_shell_projections_detached(
                &mut state,
                device_id,
                generation,
                unix_ms().ok().unwrap_or(0),
            );
            return Err(RemoteHubError::Offline(device_id));
        }
        if matches!(
            &reply,
            ShellTaskReply::Error(error) if error.code == ShellTaskErrorCode::NotFound
        ) {
            state.shell_tasks.remove(&task_id);
        }
        Ok((device_id, reply))
    }

    pub fn disconnect(&self, device_id: DeviceId) {
        let disconnected_unix_ms = unix_ms().ok();
        let changed = if let Ok(mut state) = self.inner.lock()
            && let Some(slot) = state.sessions.remove(&device_id)
        {
            mark_shell_projections_detached(
                &mut state,
                device_id,
                slot.generation,
                disconnected_unix_ms.unwrap_or(0),
            );
            if let Some(info) = state.session_info.get_mut(&device_id)
                && info.generation == slot.generation
            {
                info.state = RemoteSessionState::Disconnected;
                if let Some(disconnected_unix_ms) = disconnected_unix_ms {
                    info.last_transition_unix_ms = disconnected_unix_ms;
                }
            }
            let _ = slot.tx.try_send(RemoteCommand::Stop);
            true
        } else {
            false
        };
        if changed {
            self.signal_session_change();
        }
    }

    fn register(
        &self,
        device_id: DeviceId,
        topology: RemoteSessionTopology,
        path: RemotePathState,
        connected_unix_ms: u64,
    ) -> Result<(u64, mpsc::Receiver<RemoteCommand>), RemoteHubError> {
        let (tx, rx) = mpsc::channel(REMOTE_COMMAND_CAPACITY);
        let generation = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
            state.next_generation = state
                .next_generation
                .checked_add(1)
                .ok_or(RemoteHubError::GenerationOverflow)?;
            let generation = state.next_generation;
            prune_expired_shell_projections(&mut state, connected_unix_ms);
            state.session_info.insert(
                device_id,
                RemoteSessionInfo {
                    device_id,
                    generation,
                    state: RemoteSessionState::Connected,
                    topology,
                    path,
                    connected_unix_ms,
                    last_transition_unix_ms: connected_unix_ms,
                    last_path_change_unix_ms: connected_unix_ms,
                },
            );
            if let Some(previous) = state
                .sessions
                .insert(device_id, RemoteSessionSlot { generation, tx })
            {
                mark_shell_projections_detached(
                    &mut state,
                    device_id,
                    previous.generation,
                    connected_unix_ms,
                );
                let _ = previous.tx.try_send(RemoteCommand::Stop);
            }
            generation
        };
        self.signal_session_change();
        Ok((generation, rx))
    }

    fn unregister(&self, device_id: DeviceId, generation: u64, disconnected_unix_ms: Option<u64>) {
        let changed = if let Ok(mut state) = self.inner.lock()
            && state
                .sessions
                .get(&device_id)
                .is_some_and(|slot| slot.generation == generation)
        {
            state.sessions.remove(&device_id);
            mark_shell_projections_detached(
                &mut state,
                device_id,
                generation,
                disconnected_unix_ms.unwrap_or(0),
            );
            if let Some(info) = state.session_info.get_mut(&device_id)
                && info.generation == generation
            {
                info.state = RemoteSessionState::Disconnected;
                if let Some(disconnected_unix_ms) = disconnected_unix_ms {
                    info.last_transition_unix_ms = disconnected_unix_ms;
                }
            }
            true
        } else {
            false
        };
        if changed {
            self.signal_session_change();
        }
    }
}

fn mark_shell_projections_detached(
    state: &mut RemoteHubState,
    device_id: DeviceId,
    generation: u64,
    disconnected_unix_ms: u64,
) {
    let deadline = disconnected_unix_ms.saturating_add(SHELL_RECONNECT_GRACE_MS);
    for projection in state.shell_tasks.values_mut() {
        if projection.device_id == device_id && projection.generation == generation {
            projection.reattach_deadline_unix_ms = Some(deadline);
        }
    }
}

fn prune_expired_shell_projections(state: &mut RemoteHubState, now_unix_ms: u64) {
    state.shell_tasks.retain(|_, projection| {
        projection
            .reattach_deadline_unix_ms
            .is_none_or(|deadline| now_unix_ms <= deadline)
    });
}

async fn send_tcp_forward_command(
    tx: &mpsc::Sender<RemoteCommand>,
    device_id: DeviceId,
    request: TcpForwardRequest,
) -> Result<TcpForwardReply, RemoteHubError> {
    let request_id = RequestId::new();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RemoteCommand::TcpForward {
        request_id,
        request,
        reply: reply_tx,
    })
    .await
    .map_err(|_| RemoteHubError::Offline(device_id))?;
    reply_rx
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?
}

fn validate_tcp_forward_reply_correlation(
    request: &TcpForwardRequest,
    reply: &TcpForwardReply,
) -> Result<(), RemoteHubError> {
    let matches = match (request, reply) {
        (
            TcpForwardRequest::Open { connection_id, .. },
            TcpForwardReply::Opened {
                connection_id: actual,
            },
        ) => actual == connection_id,
        (
            TcpForwardRequest::Exchange { connection_id, .. },
            TcpForwardReply::Exchanged {
                connection_id: actual,
                ..
            },
        ) => actual == connection_id,
        (
            TcpForwardRequest::Close { connection_id },
            TcpForwardReply::Closed {
                connection_id: actual,
            },
        ) => actual == connection_id,
        (_, TcpForwardReply::Error(_)) => true,
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(RemoteHubError::TcpForwardReplyMismatch)
    }
}
async fn send_file_transfer_command(
    tx: &mpsc::Sender<RemoteCommand>,
    device_id: DeviceId,
    request: FileTransferRequest,
) -> Result<FileTransferReply, RemoteHubError> {
    let request_id = RequestId::new();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RemoteCommand::FileTransfer {
        request_id,
        request,
        reply: reply_tx,
    })
    .await
    .map_err(|_| RemoteHubError::Offline(device_id))?;
    reply_rx
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?
}

async fn send_shell_task_command(
    tx: &mpsc::Sender<RemoteCommand>,
    device_id: DeviceId,
    request: ShellTaskRequest,
) -> Result<ShellTaskReply, RemoteHubError> {
    let request_id = RequestId::new();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RemoteCommand::ShellTask {
        request_id,
        request,
        reply: reply_tx,
    })
    .await
    .map_err(|_| RemoteHubError::Offline(device_id))?;
    reply_rx
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?
}

pub async fn handle_remote_connection(
    protocol: IrohProtocol,
    mut stream: IrohStream,
    identity: StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    hub: RemoteHub,
) -> Result<(), RemoteConnectionError> {
    match protocol {
        IrohProtocol::Bootstrap => handle_bootstrap(&mut stream, &identity, control).await,
        IrohProtocol::InnerSession => {
            handle_member(
                &mut stream,
                &identity,
                control,
                hub,
                None,
                true,
                RemoteSessionTopology::Direct,
            )
            .await
        }
        IrohProtocol::Connector => handle_connector(&mut stream, &identity, control, hub).await,
    }
}

enum BootstrapChannel<'a> {
    Direct(&'a mut IrohStream),
    Sealed {
        stream: &'a mut IrohStream,
        session: SealedBootstrapSession,
    },
}

impl BootstrapChannel<'_> {
    async fn send<T: Serialize>(&mut self, value: &T) -> Result<(), RemoteConnectionError> {
        match self {
            Self::Direct(stream) => Ok(write_bootstrap(&mut **stream, value).await?),
            Self::Sealed { stream, session } => Ok(session.send(&mut **stream, value).await?),
        }
    }

    async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, RemoteConnectionError> {
        match self {
            Self::Direct(stream) => Ok(read_bootstrap(&mut **stream).await?),
            Self::Sealed { stream, session } => Ok(session.recv(&mut **stream).await?),
        }
    }

    fn is_sealed(&self) -> bool {
        matches!(self, Self::Sealed { .. })
    }

    fn stream_mut(&mut self) -> &mut IrohStream {
        match self {
            Self::Direct(stream) => stream,
            Self::Sealed { stream, .. } => stream,
        }
    }
}

async fn handle_connector(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    hub: RemoteHub,
) -> Result<(), RemoteConnectionError> {
    let request = read_connector_open(stream).await?;
    let site_tag = request.validate()?;
    let site_id =
        resolve_connector_site(&control, identity.public_identity().controller_id, site_tag)?;
    match request.purpose {
        ConnectorTunnelPurpose::Bootstrap => {
            handle_sealed_bootstrap(stream, identity, control, site_id).await
        }
        ConnectorTunnelPurpose::InnerSession => {
            handle_member(
                stream,
                identity,
                control,
                hub,
                Some(site_tag),
                false,
                RemoteSessionTopology::Connector,
            )
            .await
        }
    }
}

fn resolve_connector_site(
    control: &Arc<Mutex<ControllerControlStore>>,
    controller_id: clew_core::ControllerId,
    expected_tag: SiteDiscoveryTag,
) -> Result<SiteId, RemoteConnectionError> {
    let store = control
        .lock()
        .map_err(|_| RemoteConnectionError::StatePoisoned)?;
    let mut matches = store
        .snapshot()
        .catalog
        .sites
        .values()
        .filter(|site| {
            !site.revoked && SiteDiscoveryTag::derive(controller_id, site.site_id) == expected_tag
        })
        .map(|site| site.site_id);
    let site_id = matches.next().ok_or(RemoteConnectionError::Denied)?;
    if matches.next().is_some() {
        return Err(RemoteConnectionError::Denied);
    }
    Ok(site_id)
}

async fn handle_bootstrap(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
) -> Result<(), RemoteConnectionError> {
    let mut channel = BootstrapChannel::Direct(stream);
    handle_bootstrap_channel(&mut channel, identity, control, None).await
}

async fn handle_sealed_bootstrap(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    site_id: SiteId,
) -> Result<(), RemoteConnectionError> {
    let context = SealedBootstrapContext {
        controller_id: identity.public_identity().controller_id,
        site_id,
    };
    let session =
        SealedBootstrapSession::accept(stream, context, identity.bootstrap_noise_static_secret())
            .await?;
    let mut channel = BootstrapChannel::Sealed { stream, session };
    handle_bootstrap_channel(&mut channel, identity, control, Some(site_id)).await
}

async fn handle_bootstrap_channel(
    channel: &mut BootstrapChannel<'_>,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    expected_site_id: Option<SiteId>,
) -> Result<(), RemoteConnectionError> {
    let result = handle_bootstrap_inner(channel, identity, control, expected_site_id).await;
    if let Err(error) = &result
        && let Some(body) = error.bootstrap_error_body()
    {
        send_bootstrap_rejection(channel, body).await;
    }
    result
}

async fn send_bootstrap_rejection(channel: &mut BootstrapChannel<'_>, body: BootstrapErrorBody) {
    if channel.send(&BootstrapResponse::Error(body)).await.is_err() {
        return;
    }
    let stream = channel.stream_mut();
    let _ = tokio::io::AsyncWriteExt::shutdown(stream).await;
    let mut scratch = [0_u8; 1];
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::io::AsyncReadExt::read(stream, &mut scratch),
    )
    .await;
}

async fn handle_bootstrap_inner(
    channel: &mut BootstrapChannel<'_>,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    expected_site_id: Option<SiteId>,
) -> Result<(), RemoteConnectionError> {
    if remote_access_paused(&control)? {
        return Err(RemoteConnectionError::RecoveryReviewRequired);
    }
    let first: BootstrapRequest = channel.recv().await?;
    match first {
        BootstrapRequest::Claim {
            bootstrap,
            device_identity,
            hostname,
            mode,
        } => {
            validate_hostname(&hostname)?;
            let controller = bootstrap.verify()?;
            if controller != identity.public_identity() {
                return Err(RemoteConnectionError::Denied);
            }
            let site_id = bootstrap.payload.site_id;
            if expected_site_id.is_some_and(|expected| expected != site_id) {
                return Err(RemoteConnectionError::Denied);
            }
            let site_name = bootstrap.payload.site_name.clone();
            let receipt = {
                let mut store = control
                    .lock()
                    .map_err(|_| RemoteConnectionError::StatePoisoned)?;
                let site = store
                    .snapshot()
                    .catalog
                    .site(site_id)
                    .ok_or(RemoteConnectionError::Denied)?;
                if site.revoked || site.site_name != site_name {
                    return Err(RemoteConnectionError::Denied);
                }
                let now = unix_ms()?;
                let claim_ceiling = match mode {
                    BootstrapMemberMode::ExecutePreferred => {
                        clew_identity::PermissionGrant::EXECUTE_READ_WRITE_SHELL_TCP_CONNECTOR
                    }
                    BootstrapMemberMode::ConnectorOnly => {
                        clew_identity::PermissionGrant::CONNECTOR_ONLY
                    }
                };
                store.transaction(|snapshot| {
                    Ok(snapshot.registry.claim_with_ceiling(
                        &bootstrap,
                        device_identity,
                        now,
                        claim_ceiling,
                    )?)
                })?
            };
            channel
                .send(&BootstrapResponse::Claimed(receipt.clone()))
                .await?;

            let persisted: BootstrapRequest = channel.recv().await?;
            let (invite_id, device_id, persist_ack_token, persisted_hostname) = match persisted {
                BootstrapRequest::Persisted {
                    invite_id,
                    device_id,
                    persist_ack_token,
                    hostname,
                } => (invite_id, device_id, persist_ack_token, hostname),
                _ => return Err(RemoteConnectionError::InvalidBootstrapSequence),
            };
            if invite_id != receipt.invite_id
                || device_id != receipt.device_id
                || normalize_hostname(&persisted_hostname) != normalize_hostname(&hostname)
            {
                return Err(RemoteConnectionError::Denied);
            }
            let canonical = finalize_persisted(
                &control,
                invite_id,
                device_id,
                &persist_ack_token,
                &hostname,
            )?;
            if expected_site_id.is_some_and(|expected| expected != canonical.site_id) {
                return Err(RemoteConnectionError::Denied);
            }
            channel
                .send(&BootstrapResponse::Activated(canonical))
                .await?;
            expect_activation_ack(channel, invite_id, device_id).await?;
            Ok(())
        }
        BootstrapRequest::Persisted {
            invite_id,
            device_id,
            persist_ack_token,
            hostname,
        } => {
            validate_hostname(&hostname)?;
            let canonical = finalize_persisted(
                &control,
                invite_id,
                device_id,
                &persist_ack_token,
                &hostname,
            )?;
            if expected_site_id.is_some_and(|expected| expected != canonical.site_id) {
                return Err(RemoteConnectionError::Denied);
            }
            channel
                .send(&BootstrapResponse::Activated(canonical))
                .await?;
            expect_activation_ack(channel, invite_id, device_id).await?;
            Ok(())
        }
        BootstrapRequest::ActivatedAck { .. } | BootstrapRequest::ActivationConfirmedAck { .. } => {
            Err(RemoteConnectionError::InvalidBootstrapSequence)
        }
    }
}

async fn expect_activation_ack(
    channel: &mut BootstrapChannel<'_>,
    invite_id: clew_core::InviteId,
    device_id: DeviceId,
) -> Result<(), RemoteConnectionError> {
    match channel.recv::<BootstrapRequest>().await? {
        BootstrapRequest::ActivatedAck {
            invite_id: actual_invite,
            device_id: actual_device,
        } if actual_invite == invite_id && actual_device == device_id => {
            if channel.is_sealed() {
                channel
                    .send(&BootstrapResponse::ActivationConfirmed {
                        invite_id,
                        device_id,
                    })
                    .await?;
                let _ = tokio::time::timeout(
                    SEALED_ACTIVATION_DRAIN_TIMEOUT,
                    channel.recv::<BootstrapRequest>(),
                )
                .await;
            }
            Ok(())
        }
        BootstrapRequest::ActivatedAck { .. } => Err(RemoteConnectionError::Denied),
        _ => Err(RemoteConnectionError::InvalidBootstrapSequence),
    }
}

fn validate_hostname(hostname: &str) -> Result<(), RemoteConnectionError> {
    if hostname.len() > 512 {
        return Err(RemoteConnectionError::InvalidHostname);
    }
    Ok(())
}

fn finalize_persisted(
    control: &Arc<Mutex<ControllerControlStore>>,
    invite_id: clew_core::InviteId,
    device_id: DeviceId,
    persist_ack_token: &[u8; 32],
    hostname: &str,
) -> Result<DeviceRecord, RemoteConnectionError> {
    let normalized_hostname = normalize_hostname(hostname);
    let mut store = control
        .lock()
        .map_err(|_| RemoteConnectionError::StatePoisoned)?;
    Ok(store.transaction(|snapshot| {
        let enrollment =
            snapshot
                .registry
                .finalize_host_persist(invite_id, device_id, persist_ack_token)?;
        let site = snapshot.catalog.site(enrollment.site_id).ok_or(
            clew_core::ControlModelError::UnknownSite(enrollment.site_id),
        )?;
        if site.revoked {
            return Err(clew_core::ControlModelError::UnknownSite(enrollment.site_id).into());
        }
        if let Some(existing) = snapshot.catalog.device(device_id) {
            if existing.device.site_id != enrollment.site_id
                || existing.device.enrolled_via_invite_id != enrollment.invite_id
                || existing.device.capabilities != enrollment.effective_grant.member
                || existing.revoked
            {
                return Err(clew_core::ControlModelError::DeviceConflict(device_id).into());
            }
            return Ok(existing.device.clone());
        }
        let record = DeviceRecord {
            device_id,
            site_id: enrollment.site_id,
            display_name: normalized_hostname.clone(),
            hostname_observed: normalized_hostname.clone(),
            capabilities: enrollment.effective_grant.member,
            enrolled_via_invite_id: enrollment.invite_id,
            name_origin: DeviceNameOrigin::Automatic {
                base_hostname: normalized_hostname,
                tagged: false,
                tag_generation: 0,
            },
        };
        snapshot.catalog.register_device(record.clone())?;
        Ok(record)
    })?)
}

async fn recv_rpc_reply_with_progress(
    inner: &mut InnerSession,
    stream: &mut IrohStream,
    request_id: RequestId,
    continuity: &mut SessionContinuity,
) -> Result<InnerMessage, RemoteHubError> {
    loop {
        continuity.ensure_current(hub_unix_ms()?)?;
        let message = tokio::time::timeout(SESSION_HEARTBEAT_TIMEOUT, inner.recv(stream))
            .await
            .map_err(|_| RemoteHubError::RpcLivenessTimeout(request_id))??;
        continuity.observe_peer_activity(hub_unix_ms()?)?;
        if is_rpc_progress(request_id, &message)? {
            continue;
        }
        return Ok(unwrap_rpc_reply(request_id, &message)?);
    }
}

async fn send_rpc_request_with_timeout(
    inner: &mut InnerSession,
    stream: &mut IrohStream,
    request_id: RequestId,
    message: InnerMessage,
    continuity: &SessionContinuity,
) -> Result<(), RemoteHubError> {
    continuity.ensure_current(hub_unix_ms()?)?;
    tokio::time::timeout(SESSION_HEARTBEAT_TIMEOUT, inner.send(stream, &message))
        .await
        .map_err(|_| RemoteHubError::RpcLivenessTimeout(request_id))??;
    Ok(())
}

fn hub_unix_ms() -> Result<u64, RemoteHubError> {
    unix_ms().map_err(|_| RemoteHubError::SessionContinuityLost)
}

async fn handle_member(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    hub: RemoteHub,
    expected_site_tag: Option<SiteDiscoveryTag>,
    issue_connector_lease: bool,
    topology: RemoteSessionTopology,
) -> Result<(), RemoteConnectionError> {
    let controller_id = identity.public_identity().controller_id;
    let authority = ControllerSessionAuthority::from_stored(identity);
    let control_for_auth = Arc::clone(&control);
    let mut inner = InnerSession::accept_authorized(stream, authority, move |claim| {
        let Ok(store) = control_for_auth.lock() else {
            return false;
        };
        if store
            .snapshot()
            .recovery_review
            .is_some_and(|review| review.remote_access_paused)
        {
            return false;
        }
        let Some(site) = store.snapshot().catalog.site(claim.site_id) else {
            return false;
        };
        if expected_site_tag
            .is_some_and(|tag| SiteDiscoveryTag::derive(controller_id, claim.site_id) != tag)
        {
            return false;
        };
        let Some(catalog_device) = store.snapshot().catalog.device(claim.device_id) else {
            return false;
        };
        let Some(enrollment) = store.snapshot().registry.device(claim.device_id) else {
            return false;
        };
        !site.revoked
            && !catalog_device.revoked
            && store.snapshot().registry.is_device_active(claim.device_id)
            && catalog_device.device.site_id == claim.site_id
            && enrollment.device_public_identity == claim.device_identity
    })
    .await?;
    let device_id = inner.device_id();
    let site_id = inner.site_id();
    if issue_connector_lease && connector_capability_enabled(&control, device_id, site_id)? {
        let issued_unix_ms = unix_ms()?;
        let expires_unix_ms = issued_unix_ms
            .checked_add(CONNECTOR_LEASE_TTL_MS)
            .ok_or(RemoteConnectionError::ClockOverflow)?;
        let lease = SignedConnectorLease::issue(
            identity.identity(),
            site_id,
            device_id,
            stream.connection().remote_id(),
            issued_unix_ms,
            expires_unix_ms,
        )?;
        inner.send(stream, &lease.into_message()?).await?;
    }
    let connected_unix_ms = unix_ms()?;
    let mut continuity = SessionContinuity::new(connected_unix_ms);
    let connection = stream.connection().clone();
    let initial_path = match topology {
        RemoteSessionTopology::Direct => classify_connection_path(&connection),
        RemoteSessionTopology::Connector => RemotePathState::MixedOrUnknown,
    };
    let (generation, mut commands) =
        hub.register(device_id, topology, initial_path, connected_unix_ms)?;
    let path_watcher = if topology == RemoteSessionTopology::Direct {
        let path_connection = connection.clone();
        let path_hub = hub.clone();
        Some(tokio::spawn(async move {
            let events = path_connection.path_events();
            tokio::pin!(events);
            while events.next().await.is_some() {
                if let Ok(now_unix_ms) = unix_ms() {
                    path_hub.update_path(
                        device_id,
                        generation,
                        classify_connection_path(&path_connection),
                        now_unix_ms,
                    );
                }
            }
        }))
    } else {
        None
    };
    let connection_closed = connection.closed();
    tokio::pin!(connection_closed);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + SESSION_HEARTBEAT_INTERVAL,
        SESSION_HEARTBEAT_INTERVAL,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let command = tokio::select! {
            _ = &mut connection_closed => break,
            _ = heartbeat.tick() => {
                if continuity.ensure_current(unix_ms()?).is_err() {
                    break;
                }
                let payload = generation.to_le_bytes().to_vec();
                let ping = InnerMessage::new("session_ping", payload.clone())?;
                let result = tokio::time::timeout(SESSION_HEARTBEAT_TIMEOUT, async {
                    inner.send(stream, &ping).await?;
                    let pong = inner.recv(stream).await?;
                    Ok::<_, clew_transport::InnerSessionError>(pong)
                })
                .await;
                match result {
                    Ok(Ok(pong)) if pong.kind == "session_pong" && pong.payload == payload => {
                        if continuity.observe_peer_activity(unix_ms()?).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
                continue;
            }
            command = commands.recv() => {
                let Some(command) = command else { break; };
                command
            }
        };
        match command {
            RemoteCommand::Read {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(ReadReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::FsQuery {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(FsQueryReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::FsMutation {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(FsMutationReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::FileTransfer {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(FileTransferReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::DirectoryTree {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(DirectoryTreeReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::TcpForward {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(TcpForwardReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::ShellTask {
                request_id,
                request,
                reply,
            } => {
                let result = tokio::select! {
                    _ = &mut connection_closed => Err(RemoteHubError::Offline(device_id)),
                    result = async {
                        let message = wrap_rpc_request(request_id, request.into_message()?)?;
                        send_rpc_request_with_timeout(
                            &mut inner,
                            stream,
                            request_id,
                            message,
                            &continuity,
                        )
                        .await?;
                        let message = recv_rpc_reply_with_progress(
                            &mut inner,
                            stream,
                            request_id,
                            &mut continuity,
                        )
                        .await?;
                        Ok(ShellTaskReply::from_message(&message)?)
                    } => result,
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::Stop => break,
        }
    }
    if let Some(path_watcher) = path_watcher {
        path_watcher.abort();
        let _ = path_watcher.await;
    }
    hub.unregister(device_id, generation, unix_ms().ok());
    Ok(())
}

fn classify_connection_path(connection: &iroh::endpoint::Connection) -> RemotePathState {
    let paths = connection.paths();
    let mut selected = paths.iter().filter(|path| path.is_selected());
    let Some(path) = selected.next() else {
        return RemotePathState::MixedOrUnknown;
    };
    if selected.next().is_some() {
        return RemotePathState::MixedOrUnknown;
    }
    if path.is_ip() {
        RemotePathState::Direct
    } else if path.is_relay() {
        RemotePathState::Relay
    } else {
        RemotePathState::MixedOrUnknown
    }
}

fn connector_capability_enabled(
    control: &Arc<Mutex<ControllerControlStore>>,
    device_id: DeviceId,
    site_id: SiteId,
) -> Result<bool, RemoteConnectionError> {
    let store = control
        .lock()
        .map_err(|_| RemoteConnectionError::StatePoisoned)?;
    let Some(site) = store.snapshot().catalog.site(site_id) else {
        return Ok(false);
    };
    let Some(device) = store.snapshot().catalog.device(device_id) else {
        return Ok(false);
    };
    Ok(!site.revoked
        && !device.revoked
        && device.device.site_id == site_id
        && device.device.capabilities.connector
        && store.snapshot().registry.is_device_active(device_id))
}

fn remote_access_paused(
    control: &Arc<Mutex<ControllerControlStore>>,
) -> Result<bool, RemoteConnectionError> {
    let store = control
        .lock()
        .map_err(|_| RemoteConnectionError::StatePoisoned)?;
    Ok(store
        .snapshot()
        .recovery_review
        .is_some_and(|review| review.remote_access_paused))
}

fn unix_ms() -> Result<u64, RemoteConnectionError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RemoteConnectionError::ClockBeforeUnixEpoch)?
        .as_millis()
        .try_into()
        .map_err(|_| RemoteConnectionError::ClockOverflow)
}

fn shell_request_task_id(request: &ShellTaskRequest) -> Option<TaskId> {
    match request {
        ShellTaskRequest::Start { .. } => None,
        ShellTaskRequest::Status { task_id }
        | ShellTaskRequest::Attach { task_id, .. }
        | ShellTaskRequest::Cancel { task_id } => Some(*task_id),
    }
}

fn validate_shell_reply_correlation(
    request: &ShellTaskRequest,
    reply: &ShellTaskReply,
) -> Result<(), RemoteHubError> {
    let expected = shell_request_task_id(request);
    let matches = match (request, reply) {
        (ShellTaskRequest::Start { .. }, ShellTaskReply::Started { .. }) => true,
        (_, ShellTaskReply::Error(_)) => true,
        (ShellTaskRequest::Status { task_id }, ShellTaskReply::Status(status)) => {
            status.task_id == *task_id
        }
        (ShellTaskRequest::Attach { task_id, .. }, ShellTaskReply::Output(output)) => {
            output.status.task_id == *task_id
        }
        (
            ShellTaskRequest::Cancel { task_id },
            ShellTaskReply::CancelAccepted { task_id: actual },
        ) => actual == task_id,
        _ => false,
    };
    if matches {
        Ok(())
    } else if let Some(task_id) = expected {
        Err(RemoteHubError::ShellReplyTaskMismatch(task_id))
    } else {
        Err(RemoteHubError::ShellReplyMismatch)
    }
}

#[derive(Debug, Error)]
pub enum RemoteHubError {
    #[error("device {0} is offline")]
    Offline(DeviceId),
    #[error("remote hub state is poisoned")]
    StatePoisoned,
    #[error("remote session generation overflow")]
    GenerationOverflow,
    #[error("remote session generation changed: expected {expected}, got {actual}")]
    SessionGenerationChanged { expected: u64, actual: u64 },
    #[error("remote session continuity was lost across a long process pause or wall-clock jump")]
    SessionContinuityLost,
    #[error("remote RPC {0} stopped making progress")]
    RpcLivenessTimeout(RequestId),
    #[error("TCP forward connection is bound to an older or unavailable session for device {0}")]
    TcpForwardSessionChanged(DeviceId),
    #[error("TCP forward RPC used the wrong Open/Exchange/Close shape")]
    InvalidTcpForwardRequestShape,
    #[error("TCP forward Host returned the wrong connection id or result kind")]
    TcpForwardReplyMismatch,
    #[error("Controller live Shell task projection capacity is exhausted")]
    ShellProjectionCapacity,
    #[error("Shell follow-up referenced unknown or stale task {0}")]
    UnknownShellTask(TaskId),
    #[error("Shell Host returned duplicate task id {0}")]
    ShellTaskIdConflict(TaskId),
    #[error("Shell Host returned the wrong result kind")]
    ShellReplyMismatch,
    #[error("Shell Host returned a result for the wrong task; expected {0}")]
    ShellReplyTaskMismatch(TaskId),
    #[error("Shell projection request used the wrong Start/follow-up shape")]
    InvalidShellProjectionRequest,
    #[error(transparent)]
    Rpc(#[from] clew_transport::RpcProtocolError),
    #[error(transparent)]
    Inner(#[from] clew_transport::InnerSessionError),
    #[error(transparent)]
    FsQuery(#[from] clew_transport::FsQueryProtocolError),
    #[error(transparent)]
    FsMutation(#[from] clew_transport::FsMutationProtocolError),
    #[error(transparent)]
    DirectoryTree(#[from] clew_transport::DirectoryTreeError),
    #[error(transparent)]
    FileTransfer(#[from] clew_transport::FileTransferError),
    #[error(transparent)]
    TcpForward(#[from] clew_transport::TcpForwardProtocolError),
    #[error(transparent)]
    ShellTask(#[from] clew_transport::ShellTaskProtocolError),
    #[error(transparent)]
    Read(#[from] clew_transport::ReadProtocolError),
}

#[derive(Debug, Error)]
pub enum RemoteConnectionError {
    #[error(transparent)]
    Bootstrap(#[from] clew_transport::BootstrapProtocolError),
    #[error(transparent)]
    Inner(#[from] clew_transport::InnerSessionError),
    #[error(transparent)]
    Read(#[from] clew_transport::ReadProtocolError),
    #[error(transparent)]
    Hub(#[from] RemoteHubError),
    #[error(transparent)]
    Identity(#[from] clew_identity::IdentityError),
    #[error(transparent)]
    Control(#[from] ControlStoreError),
    #[error(transparent)]
    Enrollment(#[from] clew_identity::EnrollmentError),
    #[error("Controller recovery review must be confirmed before remote access resumes")]
    RecoveryReviewRequired,
    #[error("remote connection was denied")]
    Denied,
    #[error(transparent)]
    ConnectorControl(#[from] ConnectorControlError),
    #[error(transparent)]
    SealedBootstrap(#[from] SealedBootstrapError),
    #[error(transparent)]
    ConnectorLease(#[from] ConnectorLeaseError),
    #[error("bootstrap hostname is too long")]
    InvalidHostname,
    #[error("bootstrap request sequence is invalid")]
    InvalidBootstrapSequence,
    #[error("remote controller state is poisoned")]
    StatePoisoned,
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock value does not fit in milliseconds")]
    ClockOverflow,
}

fn enrollment_bootstrap_error(error: &EnrollmentError) -> BootstrapErrorBody {
    match error {
        EnrollmentError::Json(_)
        | EnrollmentError::InvalidBootstrap(_)
        | EnrollmentError::UnsupportedBootstrapVersion(_) => BootstrapErrorBody::new(
            BootstrapErrorCode::InvalidRequest,
            "Invitation format is invalid or unsupported.",
        ),
        EnrollmentError::Identity(_)
        | EnrollmentError::BootstrapSecretMismatch
        | EnrollmentError::WrongController
        | EnrollmentError::PassConflict => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "Invitation verification failed.",
        ),
        EnrollmentError::NotYetValid => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This invitation is not valid yet.",
        ),
        EnrollmentError::Expired => {
            BootstrapErrorBody::new(BootstrapErrorCode::Denied, "This invitation has expired.")
        }
        EnrollmentError::DeploymentWindowClosed => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This Site Kit deployment window has closed.",
        ),
        EnrollmentError::InviteClosed => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This invitation is closed to new devices.",
        ),
        EnrollmentError::InviteRevoked => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This invitation or enrollment has been revoked.",
        ),
        EnrollmentError::Exhausted => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This invitation has reached its device limit.",
        ),
        EnrollmentError::FinalizedReplay => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "This device is already enrolled; reopen its existing Clew Host state.",
        ),
        EnrollmentError::MissingClaim
        | EnrollmentError::UnknownDevice(_)
        | EnrollmentError::PersistTokenMismatch => BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            "Enrollment recovery could not be verified.",
        ),
    }
}

impl RemoteConnectionError {
    fn bootstrap_error_body(&self) -> Option<BootstrapErrorBody> {
        match self {
            Self::Enrollment(error) | Self::Control(ControlStoreError::Enrollment(error)) => {
                Some(enrollment_bootstrap_error(error))
            }
            Self::RecoveryReviewRequired => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::Denied,
                "Controller recovery review must be confirmed before devices can connect.",
            )),
            Self::Denied | Self::Identity(_) => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::Denied,
                "Enrollment is not permitted.",
            )),
            Self::InvalidHostname => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::InvalidRequest,
                "Device hostname is invalid.",
            )),
            Self::InvalidBootstrapSequence => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::InvalidRequest,
                "Enrollment request sequence is invalid.",
            )),
            Self::Control(_) | Self::StatePoisoned => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::State,
                "Controller enrollment state is unavailable.",
            )),
            Self::ClockBeforeUnixEpoch | Self::ClockOverflow => Some(BootstrapErrorBody::new(
                BootstrapErrorCode::Internal,
                "Controller clock is unavailable.",
            )),
            Self::Bootstrap(_)
            | Self::Inner(_)
            | Self::Read(_)
            | Self::Hub(_)
            | Self::ConnectorControl(_)
            | Self::SealedBootstrap(_)
            | Self::ConnectorLease(_) => None,
        }
    }

    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Bootstrap(_) => "bootstrap_protocol",
            Self::Inner(_) => "inner_session",
            Self::Read(_) => "read_protocol",
            Self::Hub(_) => "remote_hub",
            Self::Identity(_) => "identity",
            Self::Control(ControlStoreError::Enrollment(_)) | Self::Enrollment(_) => "enrollment",
            Self::Control(_) => "controller_state",
            Self::RecoveryReviewRequired => "recovery_review",
            Self::Denied => "denied",
            Self::ConnectorControl(_) => "connector_control",
            Self::SealedBootstrap(_) => "sealed_bootstrap",
            Self::ConnectorLease(_) => "connector_lease",
            Self::InvalidHostname => "invalid_hostname",
            Self::InvalidBootstrapSequence => "bootstrap_sequence",
            Self::StatePoisoned => "state_poisoned",
            Self::ClockBeforeUnixEpoch | Self::ClockOverflow => "clock",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        ControllerDirectoryTransferManager, ControllerDirectoryTransferPhase,
        ControllerFileTransferManager, ControllerFileTransferPhase, DirectoryTransferInfo,
        FileTransferInfo, HARD_MAX_CONTROLLER_DIRECTORY_ACTIVE_CHILDREN,
        HARD_MAX_DIRECTORY_CHILDREN_PER_TRANSFER,
    };
    use clew_transport::{
        DirectoryTreeEntry, DirectoryTreeManifest, FileConflictPolicy, FileTransferChunk,
        FileTransferDirection, FileTransferManifest, FileTransferPhase, FileTransferReply,
        FileTransferRequest, FileTransferStatus, file_sha256_hex,
    };

    fn full_put_status(
        manifest: &FileTransferManifest,
        phase: FileTransferPhase,
    ) -> FileTransferStatus {
        let mut descriptor = manifest.initial_resume_descriptor().unwrap();
        descriptor.checkpoint_revision = descriptor.checkpoint_revision.saturating_add(1);
        descriptor.confirmed_offset = manifest.total_size;
        descriptor.confirmed_prefix_sha256 = manifest.final_sha256.clone();
        let final_device_path =
            (phase == FileTransferPhase::Completed).then(|| manifest.device_path.clone());
        FileTransferStatus {
            descriptor,
            phase,
            final_device_path,
        }
    }

    #[tokio::test]
    async fn controller_file_put_recovers_by_status_before_resuming_next_chunk() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source.bin");
        let source = vec![0x5a_u8; 5_000];
        std::fs::write(&source_path, &source).unwrap();
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_000,
            )
            .unwrap();
        let manager = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let info = manager
            .start_put(
                device_id,
                site_id,
                source_path.to_string_lossy().into_owned(),
                "/device/target.bin".into(),
                4096,
                FileConflictPolicy::FailIfExists,
            )
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected PutBegin on generation one");
        };
        let FileTransferRequest::PutBegin { manifest } = request else {
            panic!("first file-transfer request must be PutBegin");
        };
        assert_eq!(manifest.transfer_id, info.transfer_id);
        let initial = manifest.initial_resume_descriptor().unwrap();
        reply
            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                descriptor: initial.clone(),
                phase: FileTransferPhase::Receiving,
                final_device_path: None,
            })))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected first PutChunk");
        };
        let FileTransferRequest::PutChunk { chunk: first_chunk } = request else {
            panic!("expected first PutChunk");
        };
        assert_eq!(first_chunk.offset, 0);
        assert_eq!(first_chunk.decode_bytes().unwrap(), source[..4096]);
        // Simulate unknown result: Host applied the chunk, but Controller never got its reply.
        drop(reply);
        hub.unregister(device_id, generation_one, Some(1_100));
        let (_generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                1_200,
            )
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands_two.recv())
                .await
                .expect("Controller did not query Host status after reconnect")
                .unwrap()
        else {
            panic!("expected Status after reconnect");
        };
        assert_eq!(
            request,
            FileTransferRequest::Status {
                transfer_id: info.transfer_id
            }
        );
        let mut after_first = initial.clone();
        after_first.checkpoint_revision = 2;
        after_first.confirmed_offset = 4096;
        after_first.confirmed_prefix_sha256 = file_sha256_hex(&source[..4096]);
        reply
            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                descriptor: after_first.clone(),
                phase: FileTransferPhase::Receiving,
                final_device_path: None,
            })))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_two.recv().await.unwrap()
        else {
            panic!("expected resumed second PutChunk");
        };
        let FileTransferRequest::PutChunk { chunk: last_chunk } = request else {
            panic!("Controller replayed the wrong request after Status recovery");
        };
        assert_eq!(last_chunk.offset, 4096);
        assert_eq!(last_chunk.decode_bytes().unwrap(), source[4096..]);
        let mut ready = after_first.clone();
        ready.checkpoint_revision = 3;
        ready.confirmed_offset = source.len() as u64;
        ready.confirmed_prefix_sha256 = manifest.final_sha256.clone();
        reply
            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                descriptor: ready.clone(),
                phase: FileTransferPhase::ReadyToFinalize,
                final_device_path: None,
            })))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_two.recv().await.unwrap()
        else {
            panic!("expected Finalize after all chunks");
        };
        assert_eq!(
            request,
            FileTransferRequest::Finalize {
                transfer_id: info.transfer_id
            }
        );
        reply
            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                descriptor: ready,
                phase: FileTransferPhase::Completed,
                final_device_path: Some(manifest.device_path.clone()),
            })))
            .unwrap();

        let status = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = manager.status(info.transfer_id).unwrap();
                if status.phase().terminal() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Controller put task did not finish");
        let FileTransferInfo::Put(status) = status else {
            panic!("expected put projection");
        };
        assert_eq!(status.phase, ControllerFileTransferPhase::Completed);
        assert_eq!(status.confirmed_offset, source.len() as u64);
        assert_eq!(
            status.final_device_path.as_deref(),
            Some("/device/target.bin")
        );
        assert!(status.error.is_none());
    }

    #[tokio::test]
    async fn controller_directory_put_survives_manager_reload_with_same_outer_and_child_ids() {
        let temp = tempfile::tempdir().unwrap();
        let state_layout = clew_core::StateLayout::new(temp.path().join("state"));
        let source_root = temp.path().join("source-tree");
        std::fs::create_dir_all(&source_root).unwrap();
        let source = vec![0x6b_u8; 5_000];
        std::fs::write(source_root.join("a.bin"), &source).unwrap();
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub_one = RemoteHub::default();
        let file_one = ControllerFileTransferManager::load_or_create(
            hub_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_one = ControllerDirectoryTransferManager::load_or_create(
            hub_one.clone(),
            file_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let (generation_one, mut commands_one) = hub_one
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                10_000,
            )
            .unwrap();
        let outer = directory_one
            .start_put(
                device_id,
                site_id,
                source_root.to_string_lossy().into_owned(),
                "/device/target-tree".into(),
                4096,
            )
            .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } =
            commands_one.recv().await.unwrap()
        else {
            panic!("expected directory PreparePut before child Put");
        };
        let DirectoryTreeRequest::PreparePut {
            manifest: outer_manifest,
        } = request
        else {
            panic!("expected PreparePut");
        };
        assert_eq!(outer_manifest.transfer_id, outer.transfer_id);
        let staging_root = format!("/device/.clew-directory-{}.part", outer.transfer_id);
        reply
            .send(Ok(DirectoryTreeReply::Prepared {
                transfer_id: outer.transfer_id,
                staging_device_root: staging_root.clone(),
            }))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected child PutBegin");
        };
        let FileTransferRequest::PutBegin {
            manifest: child_manifest,
        } = request
        else {
            panic!("expected child PutBegin");
        };
        assert_eq!(child_manifest.device_path, format!("{staging_root}/a.bin"));
        let child_id = child_manifest.transfer_id;
        let initial = child_manifest.initial_resume_descriptor().unwrap();
        reply
            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                descriptor: initial.clone(),
                phase: FileTransferPhase::Receiving,
                final_device_path: None,
            })))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected first child PutChunk");
        };
        let FileTransferRequest::PutChunk { chunk: first_chunk } = request else {
            panic!("expected child PutChunk");
        };
        assert_eq!(first_chunk.transfer_id, child_id);
        assert_eq!(first_chunk.offset, 0);
        assert_eq!(first_chunk.decode_bytes().unwrap(), source[..4096]);
        // Host durably applied the first chunk, but the Controller process died before the reply.
        drop(reply);
        let mut after_first = initial.clone();
        after_first.checkpoint_revision = 2;
        after_first.confirmed_offset = 4096;
        after_first.confirmed_prefix_sha256 = file_sha256_hex(&source[..4096]);
        let DirectoryTransferInfo::Put(before_restart) =
            directory_one.status(outer.transfer_id).unwrap()
        else {
            panic!("expected directory Put projection");
        };
        assert_eq!(before_restart.current_file_transfer_id, Some(child_id));
        assert_eq!(
            before_restart.current_relative_path.as_deref(),
            Some("a.bin")
        );
        hub_one.unregister(device_id, generation_one, Some(10_100));
        drop(directory_one);
        drop(file_one);
        drop(commands_one);
        drop(hub_one);

        let hub_two = RemoteHub::default();
        let file_two = ControllerFileTransferManager::load_or_create(
            hub_two.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_two = ControllerDirectoryTransferManager::load_or_create(
            hub_two.clone(),
            file_two.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let DirectoryTransferInfo::Put(reloaded) = directory_two.status(outer.transfer_id).unwrap()
        else {
            panic!("expected reloaded directory Put projection");
        };
        assert_eq!(reloaded.transfer_id, outer.transfer_id);
        assert_eq!(reloaded.current_file_transfer_id, Some(child_id));
        let (_generation_two, mut commands_two) = hub_two
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                10_200,
            )
            .unwrap();

        let mut child_status = FileTransferStatus {
            descriptor: after_first.clone(),
            phase: FileTransferPhase::Receiving,
            final_device_path: None,
        };
        let final_root = "/device/target-tree".to_string();
        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DirectoryTransferInfo::Put(status) =
                    directory_two.status(outer.transfer_id).unwrap()
                    && status.phase == ControllerDirectoryTransferPhase::Completed
                {
                    break status;
                }
                let command = tokio::select! {
                    command = commands_two.recv() => command.expect("replacement session closed"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                };
                match command {
                    RemoteCommand::DirectoryTree { request, reply, .. } => match request {
                        DirectoryTreeRequest::PreparePut { manifest } => {
                            assert_eq!(manifest, outer_manifest);
                            reply
                                .send(Ok(DirectoryTreeReply::Prepared {
                                    transfer_id: outer.transfer_id,
                                    staging_device_root: staging_root.clone(),
                                }))
                                .unwrap();
                        }
                        DirectoryTreeRequest::FinalizePut { manifest } => {
                            assert_eq!(manifest, outer_manifest);
                            reply
                                .send(Ok(DirectoryTreeReply::Completed {
                                    transfer_id: outer.transfer_id,
                                    final_device_root: final_root.clone(),
                                }))
                                .unwrap();
                        }
                        other => panic!("unexpected directory request after reload: {other:?}"),
                    },
                    RemoteCommand::FileTransfer { request, reply, .. } => match request {
                        FileTransferRequest::PutBegin { manifest } => {
                            assert_eq!(manifest, child_manifest);
                            reply
                                .send(Ok(FileTransferReply::Status(child_status.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::Status { transfer_id } => {
                            assert_eq!(transfer_id, child_id);
                            reply
                                .send(Ok(FileTransferReply::Status(child_status.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::PutChunk { chunk } => {
                            assert_eq!(chunk.transfer_id, child_id);
                            assert_eq!(chunk.offset, 4096);
                            assert_eq!(chunk.decode_bytes().unwrap(), source[4096..]);
                            let mut ready = after_first.clone();
                            ready.checkpoint_revision = 3;
                            ready.confirmed_offset = source.len() as u64;
                            ready.confirmed_prefix_sha256 = child_manifest.final_sha256.clone();
                            child_status = FileTransferStatus {
                                descriptor: ready,
                                phase: FileTransferPhase::ReadyToFinalize,
                                final_device_path: None,
                            };
                            reply
                                .send(Ok(FileTransferReply::Status(child_status.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::Finalize { transfer_id } => {
                            assert_eq!(transfer_id, child_id);
                            child_status.phase = FileTransferPhase::Completed;
                            child_status.final_device_path =
                                Some(child_manifest.device_path.clone());
                            reply
                                .send(Ok(FileTransferReply::Status(child_status.clone())))
                                .unwrap();
                        }
                        other => panic!("unexpected child request after reload: {other:?}"),
                    },
                    other => panic!("unexpected remote command after reload: {other:?}"),
                }
            }
        })
        .await;
        let completed = match completed {
            Ok(status) => status,
            Err(error) => {
                let directory_status = directory_two.status(outer.transfer_id).unwrap();
                let child_status = file_two.status(child_id);
                panic!(
                    "directory Put did not complete after manager reload: {error:?}; directory={directory_status:?}; child={child_status:?}"
                );
            }
        };
        assert_eq!(completed.transfer_id, outer.transfer_id);
        assert_eq!(completed.completed_files, 1);
        assert_eq!(completed.confirmed_file_bytes, source.len() as u64);
        assert_eq!(
            completed.final_device_root.as_deref(),
            Some(final_root.as_str())
        );
        assert!(completed.error.is_none());
    }

    #[tokio::test]
    async fn controller_directory_put_restart_preserves_multi_child_window_and_completion_flags() {
        let temp = tempfile::tempdir().unwrap();
        let state_layout = clew_core::StateLayout::new(temp.path().join("state"));
        let source_root = temp.path().join("multi-restart-tree");
        std::fs::create_dir_all(&source_root).unwrap();
        for (name, byte) in [
            ("a.bin", 0x15_u8),
            ("b.bin", 0x26),
            ("c.bin", 0x37),
            ("d.bin", 0x48),
        ] {
            std::fs::write(source_root.join(name), vec![byte; 5_000]).unwrap();
        }
        let source_bytes = |path: &str| -> Vec<u8> {
            let byte = if path.ends_with("/a.bin") {
                0x15
            } else if path.ends_with("/b.bin") {
                0x26
            } else if path.ends_with("/c.bin") {
                0x37
            } else if path.ends_with("/d.bin") {
                0x48
            } else {
                panic!("unexpected multi-restart device path: {path}");
            };
            vec![byte; 5_000]
        };
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub_one = RemoteHub::default();
        let file_one = ControllerFileTransferManager::load_or_create(
            hub_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_one = ControllerDirectoryTransferManager::load_or_create(
            hub_one.clone(),
            file_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let (generation_one, mut commands_one) = hub_one
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                11_500,
            )
            .unwrap();
        let outer = directory_one
            .start_put(
                device_id,
                site_id,
                source_root.to_string_lossy().into_owned(),
                "/device/multi-restart-tree".into(),
                4096,
            )
            .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } =
            commands_one.recv().await.unwrap()
        else {
            panic!("expected directory PreparePut");
        };
        let DirectoryTreeRequest::PreparePut {
            manifest: outer_manifest,
        } = request
        else {
            panic!("expected PreparePut");
        };
        let staging_root = format!("/device/.clew-multi-restart-{}.part", outer.transfer_id);
        reply
            .send(Ok(DirectoryTreeReply::Prepared {
                transfer_id: outer.transfer_id,
                staging_device_root: staging_root.clone(),
            }))
            .unwrap();

        let reserved = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directory_one.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.active_children.len() == 4 {
                    break info.active_children;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("four child IDs were not reserved before restart");
        let reserved_ids = reserved
            .iter()
            .map(|child| child.transfer_id)
            .collect::<BTreeSet<_>>();
        let first_id = reserved[0].transfer_id;
        let mut child_manifests = Vec::new();
        let mut first_initial = None;
        let mut first_chunk_reply = None;
        let mut first_chunk = None;
        let mut begun = BTreeSet::new();
        while begun.len() < 4 || first_chunk_reply.is_none() {
            let command = tokio::time::timeout(Duration::from_secs(3), commands_one.recv())
                .await
                .expect("multi-child PutBegin set did not arrive")
                .unwrap();
            let RemoteCommand::FileTransfer { request, reply, .. } = command else {
                panic!("expected file-transfer command before restart: {command:?}");
            };
            match request {
                FileTransferRequest::PutBegin { manifest } => {
                    assert!(reserved_ids.contains(&manifest.transfer_id));
                    assert!(begun.insert(manifest.transfer_id));
                    child_manifests.push(manifest.clone());
                    if manifest.transfer_id == first_id {
                        let initial = manifest.initial_resume_descriptor().unwrap();
                        first_initial = Some(initial.clone());
                        reply
                            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                                descriptor: initial,
                                phase: FileTransferPhase::Receiving,
                                final_device_path: None,
                            })))
                            .unwrap();
                    } else {
                        reply
                            .send(Ok(FileTransferReply::Status(full_put_status(
                                &manifest,
                                FileTransferPhase::Completed,
                            ))))
                            .unwrap();
                    }
                }
                FileTransferRequest::PutChunk { chunk } if chunk.transfer_id == first_id => {
                    first_chunk = Some(chunk);
                    first_chunk_reply = Some(reply);
                }
                other => panic!("unexpected request before multi-child restart: {other:?}"),
            }
        }
        assert_eq!(begun, reserved_ids);
        let before_restart = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directory_one.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.active_children.len() == 4
                    && !info.active_children[0].completed
                    && info
                        .active_children
                        .iter()
                        .skip(1)
                        .all(|child| child.completed)
                {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("out-of-order completion flags were not persisted before restart");
        assert_eq!(before_restart.completed_files, 0);
        assert_eq!(before_restart.active_children.len(), 4);
        let first_chunk = first_chunk.unwrap();
        assert_eq!(first_chunk.offset, 0);
        assert_eq!(first_chunk.decode_bytes().unwrap().len(), 4096);
        drop(first_chunk_reply.take());
        let first_manifest = child_manifests
            .iter()
            .find(|manifest| manifest.transfer_id == first_id)
            .unwrap()
            .clone();
        let first_source = source_bytes(&first_manifest.device_path);
        let mut after_first = first_initial.unwrap();
        after_first.checkpoint_revision = after_first.checkpoint_revision.saturating_add(1);
        after_first.confirmed_offset = 4096;
        after_first.confirmed_prefix_sha256 = file_sha256_hex(&first_source[..4096]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub_one.unregister(device_id, generation_one, Some(11_600));
        drop(directory_one);
        drop(file_one);
        drop(commands_one);
        drop(hub_one);

        let hub_two = RemoteHub::default();
        let file_two = ControllerFileTransferManager::load_or_create(
            hub_two.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_two = ControllerDirectoryTransferManager::load_or_create(
            hub_two.clone(),
            file_two.clone(),
            controller_id,
            state_layout,
        )
        .unwrap();
        let DirectoryTransferInfo::Put(reloaded) = directory_two.status(outer.transfer_id).unwrap()
        else {
            panic!("expected reloaded multi-child directory Put");
        };
        assert_eq!(reloaded.completed_files, 0);
        assert_eq!(reloaded.active_children.len(), 4);
        assert_eq!(
            reloaded
                .active_children
                .iter()
                .map(|child| child.transfer_id)
                .collect::<BTreeSet<_>>(),
            reserved_ids
        );
        assert!(!reloaded.active_children[0].completed);
        assert!(
            reloaded
                .active_children
                .iter()
                .skip(1)
                .all(|child| child.completed)
        );
        assert_eq!(reloaded.current_file_transfer_id, Some(first_id));

        let (_generation_two, mut commands_two) = hub_two
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                11_700,
            )
            .unwrap();
        let mut first_status = FileTransferStatus {
            descriptor: after_first,
            phase: FileTransferPhase::Receiving,
            final_device_path: None,
        };
        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DirectoryTransferInfo::Put(info) =
                    directory_two.status(outer.transfer_id).unwrap()
                    && info.phase == ControllerDirectoryTransferPhase::Completed
                {
                    break info;
                }
                let command = tokio::select! {
                    command = commands_two.recv() => command.expect("replacement session closed"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                };
                match command {
                    RemoteCommand::DirectoryTree { request, reply, .. } => match request {
                        DirectoryTreeRequest::PreparePut { manifest } => {
                            assert_eq!(manifest, outer_manifest);
                            reply
                                .send(Ok(DirectoryTreeReply::Prepared {
                                    transfer_id: outer.transfer_id,
                                    staging_device_root: staging_root.clone(),
                                }))
                                .unwrap();
                        }
                        DirectoryTreeRequest::FinalizePut { manifest } => {
                            assert_eq!(manifest, outer_manifest);
                            reply
                                .send(Ok(DirectoryTreeReply::Completed {
                                    transfer_id: outer.transfer_id,
                                    final_device_root: "/device/multi-restart-tree".into(),
                                }))
                                .unwrap();
                        }
                        other => panic!(
                            "unexpected directory request after multi-child restart: {other:?}"
                        ),
                    },
                    RemoteCommand::FileTransfer { request, reply, .. } => match request {
                        FileTransferRequest::PutBegin { manifest } => {
                            assert!(reserved_ids.contains(&manifest.transfer_id));
                            if manifest.transfer_id == first_id {
                                assert_eq!(manifest, first_manifest);
                                reply
                                    .send(Ok(FileTransferReply::Status(first_status.clone())))
                                    .unwrap();
                            } else {
                                let original = child_manifests
                                    .iter()
                                    .find(|original| original.transfer_id == manifest.transfer_id)
                                    .unwrap();
                                assert_eq!(&manifest, original);
                                reply
                                    .send(Ok(FileTransferReply::Status(full_put_status(
                                        &manifest,
                                        FileTransferPhase::Completed,
                                    ))))
                                    .unwrap();
                            }
                        }
                        FileTransferRequest::Status { transfer_id } => {
                            assert_eq!(transfer_id, first_id);
                            reply
                                .send(Ok(FileTransferReply::Status(first_status.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::PutChunk { chunk } => {
                            assert_eq!(chunk.transfer_id, first_id);
                            assert_eq!(chunk.offset, 4096);
                            assert_eq!(chunk.decode_bytes().unwrap(), first_source[4096..]);
                            let mut descriptor = first_status.descriptor.clone();
                            descriptor.checkpoint_revision =
                                descriptor.checkpoint_revision.saturating_add(1);
                            descriptor.confirmed_offset = first_manifest.total_size;
                            descriptor.confirmed_prefix_sha256 =
                                first_manifest.final_sha256.clone();
                            first_status = FileTransferStatus {
                                descriptor,
                                phase: FileTransferPhase::ReadyToFinalize,
                                final_device_path: None,
                            };
                            reply
                                .send(Ok(FileTransferReply::Status(first_status.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::Finalize { transfer_id } => {
                            assert_eq!(transfer_id, first_id);
                            first_status.phase = FileTransferPhase::Completed;
                            first_status.final_device_path =
                                Some(first_manifest.device_path.clone());
                            reply
                                .send(Ok(FileTransferReply::Status(first_status.clone())))
                                .unwrap();
                        }
                        other => {
                            panic!("unexpected child request after multi-child restart: {other:?}")
                        }
                    },
                    other => panic!("unexpected command after multi-child restart: {other:?}"),
                }
            }
        })
        .await
        .expect("multi-child directory Put did not complete after manager restart");
        assert_eq!(completed.transfer_id, outer.transfer_id);
        assert_eq!(completed.completed_files, 4);
        assert_eq!(completed.confirmed_file_bytes, 20_000);
        assert!(completed.active_children.is_empty());
    }

    #[tokio::test]
    async fn controller_directory_cancel_covers_every_in_flight_child() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("cancel-window-tree");
        std::fs::create_dir_all(&source_root).unwrap();
        for (name, byte) in [
            ("a.bin", 0x19_u8),
            ("b.bin", 0x2a),
            ("c.bin", 0x3b),
            ("d.bin", 0x4c),
        ] {
            std::fs::write(source_root.join(name), vec![byte; 5_000]).unwrap();
        }
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (_generation, mut commands) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                11_900,
            )
            .unwrap();
        let file_transfers = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let directories = ControllerDirectoryTransferManager::new(
            hub.clone(),
            file_transfers.clone(),
            controller_id,
        );
        let outer = directories
            .start_put(
                device_id,
                site_id,
                source_root.to_string_lossy().into_owned(),
                "/device/cancel-window-tree".into(),
                4096,
            )
            .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } = commands.recv().await.unwrap()
        else {
            panic!("expected directory PreparePut");
        };
        let DirectoryTreeRequest::PreparePut {
            manifest: outer_manifest,
        } = request
        else {
            panic!("expected PreparePut");
        };
        reply
            .send(Ok(DirectoryTreeReply::Prepared {
                transfer_id: outer.transfer_id,
                staging_device_root: format!("/device/.clew-cancel-{}.part", outer.transfer_id),
            }))
            .unwrap();

        let active = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.active_children.len() == 4 {
                    break info.active_children;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("directory cancel test never filled its child window");
        let active_ids = active
            .iter()
            .map(|child| child.transfer_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(active_ids.len(), 4);

        let mut child_manifests = Vec::new();
        let mut begun = BTreeSet::new();
        while begun.len() < 4 {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("child PutBegin requests did not arrive")
                .unwrap();
            let RemoteCommand::FileTransfer { request, reply, .. } = command else {
                panic!("expected child PutBegin during cancel setup: {command:?}");
            };
            let FileTransferRequest::PutBegin { manifest } = request else {
                panic!("expected PutBegin during cancel setup: {request:?}");
            };
            assert!(active_ids.contains(&manifest.transfer_id));
            assert!(begun.insert(manifest.transfer_id));
            let initial = manifest.initial_resume_descriptor().unwrap();
            child_manifests.push(manifest);
            reply
                .send(Ok(FileTransferReply::Status(FileTransferStatus {
                    descriptor: initial,
                    phase: FileTransferPhase::Receiving,
                    final_device_path: None,
                })))
                .unwrap();
        }
        let DirectoryTransferInfo::Put(cancelled) = directories.cancel(outer.transfer_id).unwrap()
        else {
            panic!("expected cancelled directory Put projection");
        };
        assert_eq!(
            cancelled.phase,
            ControllerDirectoryTransferPhase::Cancelling
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if active_ids.iter().all(|child_id| {
                    file_transfers.status(*child_id).is_ok_and(|info| {
                        matches!(
                            info.phase(),
                            ControllerFileTransferPhase::Cancelling
                                | ControllerFileTransferPhase::Cancelled
                        )
                    })
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("outer directory Cancel did not mark every active child cancelling");

        let mut child_cancel_ids = BTreeSet::new();
        let mut directory_cleanup = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let DirectoryTransferInfo::Put(info) = directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                let all_children_cancelled = active_ids.iter().all(|child_id| {
                    file_transfers.status(*child_id).is_ok_and(|info| {
                        info.phase() == ControllerFileTransferPhase::Cancelled
                    })
                });
                if info.phase == ControllerDirectoryTransferPhase::Cancelled
                    && all_children_cancelled
                    && child_cancel_ids == active_ids
                    && directory_cleanup
                {
                    break;
                }
                let command = tokio::select! {
                    command = commands.recv() => command.expect("cancel test remote session closed"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                };
                match command {
                    RemoteCommand::FileTransfer { request, reply, .. } => match request {
                        FileTransferRequest::PutChunk { chunk } => {
                            let manifest = child_manifests
                                .iter()
                                .find(|manifest| manifest.transfer_id == chunk.transfer_id)
                                .unwrap();
                            assert_eq!(chunk.offset, 0);
                            let bytes = chunk.decode_bytes().unwrap();
                            assert_eq!(bytes.len(), 4096);
                            let mut descriptor = manifest.initial_resume_descriptor().unwrap();
                            descriptor.checkpoint_revision =
                                descriptor.checkpoint_revision.saturating_add(1);
                            descriptor.confirmed_offset = bytes.len() as u64;
                            descriptor.confirmed_prefix_sha256 = file_sha256_hex(&bytes);
                            reply
                                .send(Ok(FileTransferReply::Status(FileTransferStatus {
                                    descriptor,
                                    phase: FileTransferPhase::Receiving,
                                    final_device_path: None,
                                })))
                                .unwrap();
                        }
                        FileTransferRequest::Cancel { transfer_id } => {
                            assert!(active_ids.contains(&transfer_id));
                            child_cancel_ids.insert(transfer_id);
                            reply
                                .send(Ok(FileTransferReply::Cancelled { transfer_id }))
                                .unwrap();
                        }
                        other => panic!("unexpected child request during directory Cancel: {other:?}"),
                    },
                    RemoteCommand::DirectoryTree { request, reply, .. } => {
                        let DirectoryTreeRequest::CancelPut { manifest } = request else {
                            panic!("unexpected directory request during Cancel: {request:?}");
                        };
                        assert_eq!(manifest, outer_manifest);
                        directory_cleanup = true;
                        reply
                            .send(Ok(DirectoryTreeReply::Cancelled {
                                transfer_id: outer.transfer_id,
                            }))
                            .unwrap();
                    }
                    other => panic!("unexpected command during directory Cancel: {other:?}"),
                }
            }
        })
        .await
        .expect("directory Cancel did not clean every child and the Host staging tree");

        let DirectoryTransferInfo::Put(final_info) = directories.status(outer.transfer_id).unwrap()
        else {
            panic!("expected directory Put projection");
        };
        assert_eq!(
            final_info.phase,
            ControllerDirectoryTransferPhase::Cancelled
        );
        assert!(final_info.active_children.is_empty());
        assert!(final_info.current_file_transfer_id.is_none());
        assert_eq!(child_cancel_ids, active_ids);
    }

    #[tokio::test]
    async fn controller_directory_put_runs_four_child_window_and_preserves_contiguous_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("concurrent-tree");
        std::fs::create_dir_all(&source_root).unwrap();
        for (name, byte) in [
            ("a.bin", 0x11_u8),
            ("b.bin", 0x22),
            ("c.bin", 0x33),
            ("d.bin", 0x44),
        ] {
            std::fs::write(source_root.join(name), vec![byte; 32]).unwrap();
        }
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (_generation, mut commands) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                12_000,
            )
            .unwrap();
        let file_transfers = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let directories = ControllerDirectoryTransferManager::new(
            hub.clone(),
            file_transfers.clone(),
            controller_id,
        );
        let outer = directories
            .start_put(
                device_id,
                site_id,
                source_root.to_string_lossy().into_owned(),
                "/device/concurrent-tree".into(),
                4096,
            )
            .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } = commands.recv().await.unwrap()
        else {
            panic!("expected directory PreparePut");
        };
        let DirectoryTreeRequest::PreparePut {
            manifest: outer_manifest,
        } = request
        else {
            panic!("expected PreparePut");
        };
        assert_eq!(outer_manifest.transfer_id, outer.transfer_id);
        let staging_root = format!("/device/.clew-window-{}.part", outer.transfer_id);
        reply
            .send(Ok(DirectoryTreeReply::Prepared {
                transfer_id: outer.transfer_id,
                staging_device_root: staging_root,
            }))
            .unwrap();

        let active = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.active_children.len() == HARD_MAX_DIRECTORY_CHILDREN_PER_TRANSFER {
                    break info.active_children;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("directory scheduler did not reserve a four-child window");
        assert_eq!(active.len(), 4);
        assert_eq!(
            active
                .iter()
                .map(|child| child.file_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let first_id = active[0].transfer_id;
        let active_ids = active
            .iter()
            .map(|child| child.transfer_id)
            .collect::<BTreeSet<_>>();

        let mut begun_ids = BTreeSet::new();
        let mut first_manifest = None;
        let mut first_chunk = None;
        let mut first_chunk_reply = None;
        while begun_ids.len() < 4 || first_chunk_reply.is_none() {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("four child PutBegin requests were not all issued")
                .expect("remote session closed");
            let RemoteCommand::FileTransfer { request, reply, .. } = command else {
                panic!("expected child file-transfer command, got {command:?}");
            };
            match request {
                FileTransferRequest::PutBegin { manifest } => {
                    assert!(active_ids.contains(&manifest.transfer_id));
                    assert!(begun_ids.insert(manifest.transfer_id));
                    if manifest.transfer_id == first_id {
                        let initial = manifest.initial_resume_descriptor().unwrap();
                        first_manifest = Some(manifest);
                        reply
                            .send(Ok(FileTransferReply::Status(FileTransferStatus {
                                descriptor: initial,
                                phase: FileTransferPhase::Receiving,
                                final_device_path: None,
                            })))
                            .unwrap();
                    } else {
                        reply
                            .send(Ok(FileTransferReply::Status(full_put_status(
                                &manifest,
                                FileTransferPhase::Completed,
                            ))))
                            .unwrap();
                    }
                }
                FileTransferRequest::PutChunk { chunk } if chunk.transfer_id == first_id => {
                    assert!(first_chunk_reply.is_none());
                    first_chunk = Some(chunk);
                    first_chunk_reply = Some(reply);
                }
                other => panic!("unexpected request while opening child window: {other:?}"),
            }
        }
        assert_eq!(begun_ids, active_ids);

        let out_of_order = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.active_children.len() == 4
                    && !info.active_children[0].completed
                    && info
                        .active_children
                        .iter()
                        .skip(1)
                        .all(|child| child.completed)
                {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("later child completions were not projected into the durable window");
        assert_eq!(out_of_order.completed_files, 0);
        assert_eq!(out_of_order.current_file_transfer_id, Some(first_id));
        assert_eq!(out_of_order.confirmed_file_bytes, 96);

        let first_manifest = first_manifest.expect("first child manifest was not observed");
        let first_chunk = first_chunk.expect("first child chunk was not observed");
        assert_eq!(first_chunk.offset, 0);
        assert_eq!(first_chunk.decode_bytes().unwrap().len(), 32);
        let ready = full_put_status(&first_manifest, FileTransferPhase::ReadyToFinalize);
        first_chunk_reply
            .take()
            .unwrap()
            .send(Ok(FileTransferReply::Status(ready.clone())))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("first child Finalize was not issued")
                .unwrap()
        else {
            panic!("expected first child Finalize");
        };
        assert_eq!(
            request,
            FileTransferRequest::Finalize {
                transfer_id: first_id
            }
        );
        reply
            .send(Ok(FileTransferReply::Status(full_put_status(
                &first_manifest,
                FileTransferPhase::Completed,
            ))))
            .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("directory FinalizePut was not issued")
                .unwrap()
        else {
            panic!("expected directory FinalizePut");
        };
        let DirectoryTreeRequest::FinalizePut { manifest } = request else {
            panic!("expected FinalizePut");
        };
        assert_eq!(manifest, outer_manifest);
        reply
            .send(Ok(DirectoryTreeReply::Completed {
                transfer_id: outer.transfer_id,
                final_device_root: "/device/concurrent-tree".into(),
            }))
            .unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Put(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Put projection");
                };
                if info.phase == ControllerDirectoryTransferPhase::Completed {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("concurrent directory Put did not complete");
        assert_eq!(completed.completed_files, 4);
        assert_eq!(completed.confirmed_file_bytes, 128);
        assert!(completed.active_children.is_empty());
        assert!(completed.current_file_transfer_id.is_none());
    }

    #[tokio::test]
    async fn controller_directory_get_runs_four_child_window_and_preserves_contiguous_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let destination_root = temp.path().join("concurrent-download");
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (_generation, mut commands) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                12_500,
            )
            .unwrap();
        let file_transfers = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let directories =
            ControllerDirectoryTransferManager::new(hub.clone(), file_transfers, controller_id);
        let outer = directories
            .start_get(
                device_id,
                site_id,
                "/device/concurrent-source".into(),
                destination_root.to_string_lossy().into_owned(),
                4096,
            )
            .unwrap();
        let file_bytes = |path: &str| -> Vec<u8> {
            let byte = if path.ends_with("/a.bin") {
                0x51
            } else if path.ends_with("/b.bin") {
                0x62
            } else if path.ends_with("/c.bin") {
                0x73
            } else if path.ends_with("/d.bin") {
                0x84
            } else {
                panic!("unexpected test device path: {path}");
            };
            vec![byte; 32]
        };
        let outer_manifest = DirectoryTreeManifest::new(
            outer.transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/concurrent-source",
            ["a.bin", "b.bin", "c.bin", "d.bin"]
                .into_iter()
                .map(|name| {
                    let path = format!("/device/concurrent-source/{name}");
                    let bytes = file_bytes(&path);
                    DirectoryTreeEntry::file(name, bytes.len() as u64, file_sha256_hex(&bytes))
                        .unwrap()
                })
                .collect(),
            None,
        )
        .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } = commands.recv().await.unwrap()
        else {
            panic!("expected directory PrepareGet");
        };
        let DirectoryTreeRequest::PrepareGet { scope } = request else {
            panic!("expected PrepareGet");
        };
        assert_eq!(scope.transfer_id, outer.transfer_id);
        reply
            .send(Ok(DirectoryTreeReply::Manifest {
                manifest: outer_manifest.clone(),
            }))
            .unwrap();

        let active = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Get(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Get projection");
                };
                if info.active_children.len() == HARD_MAX_DIRECTORY_CHILDREN_PER_TRANSFER {
                    break info.active_children;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("directory Get scheduler did not reserve a four-child window");
        let first_id = active[0].transfer_id;
        let active_ids = active
            .iter()
            .map(|child| child.transfer_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(active_ids.len(), 4);

        let mut manifests = Vec::new();
        let mut begun_ids = BTreeSet::new();
        let mut later_chunks = BTreeSet::new();
        let mut first_chunk_reply = None;
        while begun_ids.len() < 4 || later_chunks.len() < 3 || first_chunk_reply.is_none() {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("four child Get requests were not all issued")
                .expect("remote session closed");
            let RemoteCommand::FileTransfer { request, reply, .. } = command else {
                panic!("expected child Get command, got {command:?}");
            };
            match request {
                FileTransferRequest::GetBegin {
                    transfer_id,
                    device_path,
                    chunk_size,
                } => {
                    assert_eq!(chunk_size, 4096);
                    assert!(active_ids.contains(&transfer_id));
                    assert!(begun_ids.insert(transfer_id));
                    let bytes = file_bytes(&device_path);
                    let manifest = FileTransferManifest::new(
                        transfer_id,
                        controller_id,
                        site_id,
                        device_id,
                        FileTransferDirection::DeviceToController,
                        device_path,
                        bytes.len() as u64,
                        chunk_size,
                        file_sha256_hex(&bytes),
                        None,
                    )
                    .unwrap();
                    manifests.push(manifest.clone());
                    reply
                        .send(Ok(FileTransferReply::Manifest(manifest)))
                        .unwrap();
                }
                FileTransferRequest::GetChunk {
                    transfer_id,
                    offset,
                } => {
                    assert_eq!(offset, 0);
                    let manifest = manifests
                        .iter()
                        .find(|manifest| manifest.transfer_id == transfer_id)
                        .expect("GetChunk arrived before its manifest");
                    let bytes = file_bytes(&manifest.device_path);
                    if transfer_id == first_id {
                        assert!(first_chunk_reply.is_none());
                        first_chunk_reply = Some(reply);
                    } else {
                        assert!(later_chunks.insert(transfer_id));
                        reply
                            .send(Ok(FileTransferReply::Chunk(
                                FileTransferChunk::from_bytes(transfer_id, 0, &bytes).unwrap(),
                            )))
                            .unwrap();
                    }
                }
                FileTransferRequest::Cancel { transfer_id } => {
                    reply
                        .send(Ok(FileTransferReply::Cancelled { transfer_id }))
                        .unwrap();
                }
                other => panic!("unexpected request while opening Get window: {other:?}"),
            }
        }
        assert_eq!(begun_ids, active_ids);

        let out_of_order = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Get(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Get projection");
                };
                if info.active_children.len() == 4
                    && !info.active_children[0].completed
                    && info
                        .active_children
                        .iter()
                        .skip(1)
                        .all(|child| child.completed)
                {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("later Get completions were not projected into the durable window");
        assert_eq!(out_of_order.completed_files, 0);
        assert_eq!(out_of_order.current_file_transfer_id, Some(first_id));
        assert_eq!(out_of_order.confirmed_file_bytes, 96);

        let first_manifest = manifests
            .iter()
            .find(|manifest| manifest.transfer_id == first_id)
            .unwrap();
        let first_bytes = file_bytes(&first_manifest.device_path);
        first_chunk_reply
            .take()
            .unwrap()
            .send(Ok(FileTransferReply::Chunk(
                FileTransferChunk::from_bytes(first_id, 0, &first_bytes).unwrap(),
            )))
            .unwrap();

        let mut finalized = false;
        while !finalized {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("directory Get did not reach finalization")
                .expect("remote session closed");
            match command {
                RemoteCommand::FileTransfer { request, reply, .. } => {
                    let FileTransferRequest::Cancel { transfer_id } = request else {
                        panic!("unexpected child request during Get finalization: {request:?}");
                    };
                    reply
                        .send(Ok(FileTransferReply::Cancelled { transfer_id }))
                        .unwrap();
                }
                RemoteCommand::DirectoryTree { request, reply, .. } => {
                    let DirectoryTreeRequest::FinalizeGet { manifest } = request else {
                        panic!("unexpected directory request during Get finalization: {request:?}");
                    };
                    assert_eq!(manifest, outer_manifest);
                    reply
                        .send(Ok(DirectoryTreeReply::Verified {
                            transfer_id: outer.transfer_id,
                            device_root: outer_manifest.device_root.clone(),
                        }))
                        .unwrap();
                    finalized = true;
                }
                other => panic!("unexpected command during Get finalization: {other:?}"),
            }
        }

        let completed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let DirectoryTransferInfo::Get(info) =
                    directories.status(outer.transfer_id).unwrap()
                else {
                    panic!("expected directory Get projection");
                };
                if info.phase == ControllerDirectoryTransferPhase::Completed {
                    break info;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("concurrent directory Get did not complete");
        assert_eq!(completed.completed_files, 4);
        assert_eq!(completed.confirmed_file_bytes, 128);
        assert!(completed.active_children.is_empty());
        assert!(completed.current_file_transfer_id.is_none());
        for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
            let path = format!("/device/concurrent-source/{name}");
            assert_eq!(
                std::fs::read(destination_root.join(name)).unwrap(),
                file_bytes(&path)
            );
        }
    }

    #[tokio::test]
    async fn controller_directory_child_budget_never_runs_more_than_eight_incomplete_children() {
        let temp = tempfile::tempdir().unwrap();
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (_generation, mut commands) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                13_000,
            )
            .unwrap();
        let file_transfers = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let directories =
            ControllerDirectoryTransferManager::new(hub.clone(), file_transfers, controller_id);

        let mut outers = Vec::new();
        for tree in 0..3_u8 {
            let root = temp.path().join(format!("tree-{tree}"));
            std::fs::create_dir_all(&root).unwrap();
            for file in 0..4_u8 {
                std::fs::write(
                    root.join(format!("{file}.bin")),
                    vec![tree.wrapping_mul(16).wrapping_add(file); 16],
                )
                .unwrap();
            }
            outers.push(
                directories
                    .start_put(
                        device_id,
                        site_id,
                        root.to_string_lossy().into_owned(),
                        format!("/device/budget-{tree}"),
                        4096,
                    )
                    .unwrap(),
            );
        }

        let mut prepared = 0_usize;
        let mut pending_begins = Vec::new();
        while prepared < outers.len() {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("directory PreparePut requests did not arrive")
                .expect("remote session closed");
            match command {
                RemoteCommand::DirectoryTree { request, reply, .. } => {
                    let DirectoryTreeRequest::PreparePut { manifest } = request else {
                        panic!("unexpected directory request while preparing budget test");
                    };
                    prepared += 1;
                    reply
                        .send(Ok(DirectoryTreeReply::Prepared {
                            transfer_id: manifest.transfer_id,
                            staging_device_root: format!(
                                "/device/.clew-budget-{}.part",
                                manifest.transfer_id
                            ),
                        }))
                        .unwrap();
                }
                RemoteCommand::FileTransfer { request, reply, .. } => {
                    let FileTransferRequest::PutBegin { manifest } = request else {
                        panic!("unexpected file request before all directories prepared");
                    };
                    pending_begins.push((manifest, reply));
                }
                other => panic!("unexpected command while preparing budget test: {other:?}"),
            }
        }

        let initial_ids = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut ids = BTreeSet::new();
                let mut incomplete = 0_usize;
                let mut valid = true;
                for outer in &outers {
                    let DirectoryTransferInfo::Put(info) =
                        directories.status(outer.transfer_id).unwrap()
                    else {
                        panic!("expected directory Put projection");
                    };
                    if info.active_children.len() > HARD_MAX_DIRECTORY_CHILDREN_PER_TRANSFER {
                        valid = false;
                    }
                    for child in info.active_children {
                        ids.insert(child.transfer_id);
                        if !child.completed {
                            incomplete += 1;
                        }
                    }
                }
                assert!(valid, "per-directory active child window exceeded four");
                assert!(
                    incomplete <= HARD_MAX_CONTROLLER_DIRECTORY_ACTIVE_CHILDREN,
                    "Controller-wide incomplete child budget exceeded eight"
                );
                if incomplete == HARD_MAX_CONTROLLER_DIRECTORY_ACTIVE_CHILDREN {
                    break ids;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Controller never filled its eight-child directory budget");
        assert_eq!(
            initial_ids.len(),
            HARD_MAX_CONTROLLER_DIRECTORY_ACTIVE_CHILDREN
        );

        while pending_begins.is_empty() {
            let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
                .await
                .expect("no child PutBegin arrived for budget release")
                .unwrap();
            let RemoteCommand::FileTransfer { request, reply, .. } = command else {
                panic!("expected child PutBegin while budget is full");
            };
            let FileTransferRequest::PutBegin { manifest } = request else {
                panic!("expected PutBegin while budget is full");
            };
            pending_begins.push((manifest, reply));
        }
        let release_index = pending_begins
            .iter()
            .position(|(manifest, _)| initial_ids.contains(&manifest.transfer_id))
            .unwrap_or(0);
        let (released_manifest, released_reply) = pending_begins.swap_remove(release_index);
        let released_id = released_manifest.transfer_id;
        assert!(initial_ids.contains(&released_id));
        released_reply
            .send(Ok(FileTransferReply::Status(full_put_status(
                &released_manifest,
                FileTransferPhase::Completed,
            ))))
            .unwrap();

        let new_id = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut ids = BTreeSet::new();
                let mut incomplete = 0_usize;
                for outer in &outers {
                    let DirectoryTransferInfo::Put(info) =
                        directories.status(outer.transfer_id).unwrap()
                    else {
                        panic!("expected directory Put projection");
                    };
                    assert!(info.active_children.len() <= HARD_MAX_DIRECTORY_CHILDREN_PER_TRANSFER);
                    for child in info.active_children {
                        ids.insert(child.transfer_id);
                        if !child.completed {
                            incomplete += 1;
                        }
                    }
                }
                assert!(incomplete <= HARD_MAX_CONTROLLER_DIRECTORY_ACTIVE_CHILDREN);
                if let Some(id) = ids.iter().copied().find(|id| !initial_ids.contains(id)) {
                    break id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a released global child permit never admitted the next directory child");
        assert_ne!(new_id, released_id);
    }

    #[tokio::test]
    async fn controller_directory_get_survives_manager_reload_and_commits_same_outer_transfer() {
        let temp = tempfile::tempdir().unwrap();
        let state_layout = clew_core::StateLayout::new(temp.path().join("state"));
        let destination_root = temp.path().join("download-tree");
        let source = vec![0x73_u8; 5_000];
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub_one = RemoteHub::default();
        let file_one = ControllerFileTransferManager::load_or_create(
            hub_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_one = ControllerDirectoryTransferManager::load_or_create(
            hub_one.clone(),
            file_one.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let (generation_one, mut commands_one) = hub_one
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                11_000,
            )
            .unwrap();
        let outer = directory_one
            .start_get(
                device_id,
                site_id,
                "/device/source-tree".into(),
                destination_root.to_string_lossy().into_owned(),
                4096,
            )
            .unwrap();
        let outer_manifest = DirectoryTreeManifest::new(
            outer.transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/source-tree",
            vec![
                DirectoryTreeEntry::file("a.bin", source.len() as u64, file_sha256_hex(&source))
                    .unwrap(),
            ],
            None,
        )
        .unwrap();

        let RemoteCommand::DirectoryTree { request, reply, .. } =
            commands_one.recv().await.unwrap()
        else {
            panic!("expected directory PrepareGet before child Get");
        };
        let DirectoryTreeRequest::PrepareGet { scope } = request else {
            panic!("expected PrepareGet");
        };
        assert_eq!(scope.transfer_id, outer.transfer_id);
        assert_eq!(scope.device_root, "/device/source-tree");
        reply
            .send(Ok(DirectoryTreeReply::Manifest {
                manifest: outer_manifest.clone(),
            }))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected child GetBegin");
        };
        let FileTransferRequest::GetBegin {
            transfer_id: child_id,
            device_path,
            chunk_size,
        } = request
        else {
            panic!("expected child GetBegin");
        };
        assert_eq!(device_path, "/device/source-tree/a.bin");
        assert_eq!(chunk_size, 4096);
        let child_manifest = FileTransferManifest::new(
            child_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            device_path.clone(),
            source.len() as u64,
            chunk_size,
            file_sha256_hex(&source),
            None,
        )
        .unwrap();
        reply
            .send(Ok(FileTransferReply::Manifest(child_manifest.clone())))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected first child GetChunk");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetChunk {
                transfer_id: child_id,
                offset: 0,
            }
        );
        reply
            .send(Ok(FileTransferReply::Chunk(
                FileTransferChunk::from_bytes(child_id, 0, &source[..4096]).unwrap(),
            )))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected second child GetChunk before simulated process death");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetChunk {
                transfer_id: child_id,
                offset: 4096,
            }
        );
        drop(reply);
        let DirectoryTransferInfo::Get(before_restart) =
            directory_one.status(outer.transfer_id).unwrap()
        else {
            panic!("expected directory Get projection");
        };
        assert_eq!(before_restart.current_file_transfer_id, Some(child_id));
        assert_eq!(
            before_restart.current_relative_path.as_deref(),
            Some("a.bin")
        );
        hub_one.unregister(device_id, generation_one, Some(11_100));
        drop(directory_one);
        drop(file_one);
        drop(commands_one);
        drop(hub_one);

        let hub_two = RemoteHub::default();
        let file_two = ControllerFileTransferManager::load_or_create(
            hub_two.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let directory_two = ControllerDirectoryTransferManager::load_or_create(
            hub_two.clone(),
            file_two.clone(),
            controller_id,
            state_layout.clone(),
        )
        .unwrap();
        let DirectoryTransferInfo::Get(reloaded) = directory_two.status(outer.transfer_id).unwrap()
        else {
            panic!("expected reloaded directory Get projection");
        };
        assert_eq!(reloaded.transfer_id, outer.transfer_id);
        assert_eq!(reloaded.current_file_transfer_id, Some(child_id));
        let (_generation_two, mut commands_two) = hub_two
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                11_200,
            )
            .unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DirectoryTransferInfo::Get(status) =
                    directory_two.status(outer.transfer_id).unwrap()
                    && status.phase == ControllerDirectoryTransferPhase::Completed
                {
                    break status;
                }
                let command = tokio::select! {
                    command = commands_two.recv() => command.expect("replacement session closed"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => continue,
                };
                match command {
                    RemoteCommand::DirectoryTree { request, reply, .. } => match request {
                        DirectoryTreeRequest::PrepareGet { scope } => {
                            assert_eq!(scope.transfer_id, outer.transfer_id);
                            assert_eq!(scope.device_root, "/device/source-tree");
                            reply
                                .send(Ok(DirectoryTreeReply::Manifest {
                                    manifest: outer_manifest.clone(),
                                }))
                                .unwrap();
                        }
                        DirectoryTreeRequest::FinalizeGet { manifest } => {
                            assert_eq!(manifest, outer_manifest);
                            reply
                                .send(Ok(DirectoryTreeReply::Verified {
                                    transfer_id: outer.transfer_id,
                                    device_root: outer_manifest.device_root.clone(),
                                }))
                                .unwrap();
                        }
                        other => panic!("unexpected directory Get request after reload: {other:?}"),
                    },
                    RemoteCommand::FileTransfer { request, reply, .. } => match request {
                        FileTransferRequest::GetBegin {
                            transfer_id,
                            device_path,
                            chunk_size,
                        } => {
                            assert_eq!(transfer_id, child_id);
                            assert_eq!(device_path, child_manifest.device_path);
                            assert_eq!(chunk_size, 4096);
                            reply
                                .send(Ok(FileTransferReply::Manifest(child_manifest.clone())))
                                .unwrap();
                        }
                        FileTransferRequest::GetChunk {
                            transfer_id,
                            offset,
                        } => {
                            assert_eq!(transfer_id, child_id);
                            assert_eq!(offset, 4096);
                            reply
                                .send(Ok(FileTransferReply::Chunk(
                                    FileTransferChunk::from_bytes(child_id, 4096, &source[4096..])
                                        .unwrap(),
                                )))
                                .unwrap();
                        }
                        FileTransferRequest::Cancel { transfer_id } => {
                            assert_eq!(transfer_id, child_id);
                            reply
                                .send(Ok(FileTransferReply::Cancelled {
                                    transfer_id: child_id,
                                }))
                                .unwrap();
                        }
                        other => panic!("unexpected child Get request after reload: {other:?}"),
                    },
                    other => panic!("unexpected remote command after reload: {other:?}"),
                }
            }
        })
        .await;
        let completed = match completed {
            Ok(status) => status,
            Err(error) => {
                let directory_status = directory_two.status(outer.transfer_id).unwrap();
                let child_status = file_two.status(child_id);
                panic!(
                    "directory Get did not complete after manager reload: {error:?}; directory={directory_status:?}; child={child_status:?}; destination_exists={} staging_entries={:?}",
                    destination_root.exists(),
                    std::fs::read_dir(temp.path()).ok().map(|entries| entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect::<Vec<_>>())
                );
            }
        };
        assert_eq!(completed.transfer_id, outer.transfer_id);
        assert_eq!(completed.completed_files, 1);
        assert_eq!(completed.confirmed_file_bytes, source.len() as u64);
        assert!(completed.error.is_none());
        assert_eq!(
            std::fs::read(destination_root.join("a.bin")).unwrap(),
            source
        );
        let canonical_destination = std::fs::canonicalize(&destination_root).unwrap();
        assert_eq!(
            completed.final_destination_path.as_deref(),
            Some(canonical_destination.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn controller_file_get_reproves_manifest_before_cross_generation_resume() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("download.bin");
        let source = vec![0x37_u8; 5_000];
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let hub = RemoteHub::default();
        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                2_000,
            )
            .unwrap();
        let manager = ControllerFileTransferManager::new(hub.clone(), controller_id);
        let info = manager
            .start_get(
                device_id,
                site_id,
                "/device/source.bin".into(),
                destination.to_string_lossy().into_owned(),
                4096,
                FileConflictPolicy::FailIfExists,
            )
            .unwrap();
        let manifest = FileTransferManifest::new(
            info.transfer_id,
            controller_id,
            site_id,
            device_id,
            FileTransferDirection::DeviceToController,
            "/device/source.bin",
            source.len() as u64,
            4096,
            file_sha256_hex(&source),
            None,
        )
        .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected GetBegin on generation one");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetBegin {
                transfer_id: info.transfer_id,
                device_path: "/device/source.bin".into(),
                chunk_size: 4096,
            }
        );
        reply
            .send(Ok(FileTransferReply::Manifest(manifest.clone())))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected first GetChunk");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetChunk {
                transfer_id: info.transfer_id,
                offset: 0,
            }
        );
        reply
            .send(Ok(FileTransferReply::Chunk(
                FileTransferChunk::from_bytes(info.transfer_id, 0, &source[..4096]).unwrap(),
            )))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected second GetChunk on generation one");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetChunk {
                transfer_id: info.transfer_id,
                offset: 4096,
            }
        );
        // The device could have read this chunk, but Controller never received it. Since GetChunk is
        // read-only, reconnect must re-prove the source manifest and then request the same offset.
        drop(reply);
        hub.unregister(device_id, generation_one, Some(2_100));
        let (_generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                2_200,
            )
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands_two.recv())
                .await
                .expect("Controller did not re-prove device source after reconnect")
                .unwrap()
        else {
            panic!("expected GetBegin after reconnect");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetBegin {
                transfer_id: info.transfer_id,
                device_path: "/device/source.bin".into(),
                chunk_size: 4096,
            }
        );
        reply
            .send(Ok(FileTransferReply::Manifest(manifest.clone())))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_two.recv().await.unwrap()
        else {
            panic!("expected resumed second GetChunk");
        };
        assert_eq!(
            request,
            FileTransferRequest::GetChunk {
                transfer_id: info.transfer_id,
                offset: 4096,
            }
        );
        reply
            .send(Ok(FileTransferReply::Chunk(
                FileTransferChunk::from_bytes(info.transfer_id, 4096, &source[4096..]).unwrap(),
            )))
            .unwrap();

        let RemoteCommand::FileTransfer { request, reply, .. } = commands_two.recv().await.unwrap()
        else {
            panic!("expected source cleanup Cancel after local finalize");
        };
        assert_eq!(
            request,
            FileTransferRequest::Cancel {
                transfer_id: info.transfer_id
            }
        );
        reply
            .send(Ok(FileTransferReply::Cancelled {
                transfer_id: info.transfer_id,
            }))
            .unwrap();

        let final_info = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = manager.status(info.transfer_id).unwrap();
                if status.phase().terminal() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Controller get task did not finish");
        let FileTransferInfo::Get(final_info) = final_info else {
            panic!("expected get projection");
        };
        assert_eq!(final_info.phase, ControllerFileTransferPhase::Completed);
        assert_eq!(final_info.confirmed_offset, source.len() as u64);
        assert_eq!(
            final_info.final_sha256.as_deref(),
            Some(manifest.final_sha256.as_str())
        );
        let canonical_destination = std::fs::canonicalize(&destination).unwrap();
        assert_eq!(
            final_info.final_controller_path.as_deref(),
            Some(canonical_destination.to_string_lossy().as_ref())
        );
        assert!(final_info.error.is_none());
        assert_eq!(std::fs::read(&destination).unwrap(), source);
    }

    #[tokio::test]
    async fn authenticated_connector_member_receives_endpoint_bound_controller_lease() {
        let temp = tempfile::tempdir().unwrap();
        let layout = clew_core::StateLayout::new(temp.path());
        let stored = clew_identity::ControllerIdentityStore::new(layout.clone())
            .load_or_create()
            .unwrap();
        let controller = stored.identity().clone();
        let connector_identity = clew_identity::DeviceIdentity::from_secret([121_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = clew_core::InviteId::new();
        let now = unix_ms().unwrap();

        let mut control_store =
            ControllerControlStore::load_or_create(layout, controller.controller_id()).unwrap();
        let connector_device_id = control_store
            .transaction(|snapshot| {
                let pass = snapshot.registry.issue_bootstrap(
                    &controller,
                    clew_identity::SiteBootstrapSpec {
                        site_id,
                        invite_id,
                        site_name: "Lease Lab".into(),
                        grant: clew_identity::PermissionGrant::CONNECTOR_ONLY,
                        not_before_unix_ms: now.saturating_sub(1_000),
                        expires_unix_ms: now + 60_000,
                        deployment_window_ms: 60_000,
                        max_claims: 1,
                    },
                )?;
                snapshot
                    .catalog
                    .upsert_site(clew_core::ControllerSiteRecord {
                        site_id,
                        site_name: "Lease Lab".into(),
                        read_policy: clew_core::ReadPolicy::new(
                            vec![temp.path().to_string_lossy().into_owned()],
                            4_096,
                            2_000,
                        )?,
                        revoked: false,
                    })?;
                let receipt =
                    snapshot
                        .registry
                        .claim(&pass, connector_identity.public_identity(), now)?;
                let enrollment = snapshot.registry.finalize_host_persist(
                    invite_id,
                    receipt.device_id,
                    receipt.persist_ack_token(),
                )?;
                snapshot.catalog.register_device(DeviceRecord {
                    device_id: receipt.device_id,
                    site_id,
                    display_name: "HELPER-01".into(),
                    hostname_observed: "HELPER-01".into(),
                    capabilities: enrollment.effective_grant.member,
                    enrolled_via_invite_id: invite_id,
                    name_origin: DeviceNameOrigin::Automatic {
                        base_hostname: "HELPER-01".into(),
                        tagged: false,
                        tag_generation: 0,
                    },
                })?;
                Ok(receipt.device_id)
            })
            .unwrap();

        let control = Arc::new(Mutex::new(control_store));
        let hub = RemoteHub::default();
        let controller_outer =
            clew_transport::IrohOuter::bind_direct_only_with_secret(stored.iroh_endpoint_secret())
                .await
                .unwrap();
        let controller_addr = controller_outer.addr();
        let connector_outer = clew_transport::IrohOuter::bind_direct_only().await.unwrap();
        let connector_endpoint_id = connector_outer.addr().id;

        let server = tokio::spawn({
            let controller_outer = controller_outer.clone();
            let stored = stored.clone();
            let control = Arc::clone(&control);
            let hub = hub.clone();
            async move {
                let (protocol, stream) = controller_outer.accept_classified().await.unwrap();
                handle_remote_connection(protocol, stream, stored, control, hub).await
            }
        });

        let mut stream = connector_outer.connect(controller_addr).await.unwrap();
        let mut inner = InnerSession::connect(
            &mut stream,
            clew_transport::DeviceSessionIdentity {
                identity: connector_identity,
                pinned_controller: controller.public_identity(),
                device_id: connector_device_id,
                site_id,
            },
        )
        .await
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), inner.recv(&mut stream))
            .await
            .expect("Controller did not issue a Connector lease")
            .unwrap();
        let lease = SignedConnectorLease::from_message(&message).unwrap();
        assert_eq!(
            lease
                .verify_for_candidate(
                    &controller.public_identity(),
                    site_id,
                    connector_endpoint_id,
                    unix_ms().unwrap(),
                )
                .unwrap(),
            connector_device_id
        );

        hub.disconnect(connector_device_id);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("Controller member handler did not stop")
            .unwrap()
            .unwrap();
        drop(stream);
        connector_outer.close().await;
        controller_outer.close().await;
    }

    #[test]
    fn session_continuity_rejects_long_pause_or_clock_regression() {
        let mut continuity = SessionContinuity::new(10_000);
        continuity
            .observe_peer_activity(10_000 + SESSION_CONTINUITY_GAP_MS)
            .unwrap();
        assert!(matches!(
            continuity.ensure_current(10_000 + SESSION_CONTINUITY_GAP_MS * 2 + 1),
            Err(RemoteHubError::SessionContinuityLost)
        ));

        let continuity = SessionContinuity::new(20_000);
        assert!(matches!(
            continuity.ensure_current(19_999),
            Err(RemoteHubError::SessionContinuityLost)
        ));
    }

    #[test]
    fn session_generation_and_path_telemetry_ignore_stale_updates() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let (generation_one, _commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Relay,
                1_000,
            )
            .unwrap();
        assert_eq!(
            hub.session_info(device_id).unwrap().unwrap(),
            RemoteSessionInfo {
                device_id,
                generation: generation_one,
                state: RemoteSessionState::Connected,
                topology: RemoteSessionTopology::Direct,
                path: RemotePathState::Relay,
                connected_unix_ms: 1_000,
                last_transition_unix_ms: 1_000,
                last_path_change_unix_ms: 1_000,
            }
        );

        hub.update_path(device_id, generation_one, RemotePathState::Direct, 1_100);
        let first = hub.session_info(device_id).unwrap().unwrap();
        assert_eq!(first.path, RemotePathState::Direct);
        assert_eq!(first.last_path_change_unix_ms, 1_100);

        let (generation_two, _commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                2_000,
            )
            .unwrap();
        assert!(generation_two > generation_one);
        hub.update_path(device_id, generation_one, RemotePathState::Relay, 2_100);
        hub.unregister(device_id, generation_one, Some(2_200));
        let second = hub.session_info(device_id).unwrap().unwrap();
        assert_eq!(second.generation, generation_two);
        assert_eq!(second.state, RemoteSessionState::Connected);
        assert_eq!(second.topology, RemoteSessionTopology::Connector);
        assert_eq!(second.path, RemotePathState::MixedOrUnknown);
        assert_eq!(second.last_transition_unix_ms, 2_000);

        hub.unregister(device_id, generation_two, Some(2_300));
        let disconnected = hub.session_info(device_id).unwrap().unwrap();
        assert_eq!(disconnected.generation, generation_two);
        assert_eq!(disconnected.state, RemoteSessionState::Disconnected);
        assert_eq!(disconnected.last_transition_unix_ms, 2_300);
    }

    #[tokio::test]
    async fn directory_tree_control_replays_same_request_id_on_new_generation() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let transfer_id = clew_core::TransferId::new();
        let controller_id = clew_core::ControllerId::new();
        let site_id = SiteId::new();
        let manifest = clew_transport::DirectoryTreeManifest::new(
            transfer_id,
            controller_id,
            site_id,
            device_id,
            clew_transport::FileTransferDirection::ControllerToDevice,
            "/replay/directory",
            vec![
                clew_transport::DirectoryTreeEntry::file(
                    "a.txt",
                    1,
                    clew_transport::file_sha256_hex(b"a"),
                )
                .unwrap(),
            ],
            Some(clew_transport::DirectoryConflictPolicy::FailIfExists),
        )
        .unwrap();
        let request = DirectoryTreeRequest::PreparePut {
            manifest: manifest.clone(),
        };
        let call_hub = hub.clone();
        let request_for_call = request.clone();
        let call =
            tokio::spawn(async move { call_hub.directory_tree(device_id, request_for_call).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!call.is_finished());

        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_000,
            )
            .unwrap();
        let RemoteCommand::DirectoryTree {
            request_id: first_id,
            request: first_request,
            reply: first_reply,
        } = commands_one.recv().await.unwrap()
        else {
            panic!("expected directory Prepare on first generation");
        };
        assert_eq!(first_request, request);
        drop(first_reply);
        hub.unregister(device_id, generation_one, Some(1_100));

        let (_generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_200,
            )
            .unwrap();
        let RemoteCommand::DirectoryTree {
            request_id: second_id,
            request: second_request,
            reply: second_reply,
        } = commands_two.recv().await.unwrap()
        else {
            panic!("expected directory Prepare on replacement generation");
        };
        assert_eq!(second_id, first_id);
        assert_eq!(second_request, request);
        second_reply
            .send(Ok(DirectoryTreeReply::Prepared {
                transfer_id,
                staging_device_root: "/replay/.clew-dir.part".into(),
            }))
            .unwrap();
        assert!(matches!(
            call.await.unwrap().unwrap(),
            DirectoryTreeReply::Prepared { transfer_id: actual, .. } if actual == transfer_id
        ));
    }

    #[tokio::test]
    async fn replayable_rpcs_keep_request_id_across_generation_but_shell_start_does_not_replay() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();

        let read_hub = hub.clone();
        let read_call = tokio::spawn(async move {
            read_hub
                .read(device_id, ReadRequest::new("/replay/read", 0, 16).unwrap())
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!read_call.is_finished());
        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_000,
            )
            .unwrap();
        let RemoteCommand::Read {
            request_id: read_id_one,
            reply: read_reply_one,
            ..
        } = commands_one.recv().await.unwrap()
        else {
            panic!("expected first Read attempt");
        };
        drop(read_reply_one);
        hub.unregister(device_id, generation_one, Some(1_100));
        let (generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                1_200,
            )
            .unwrap();
        let RemoteCommand::Read {
            request_id: read_id_two,
            reply: read_reply_two,
            ..
        } = tokio::time::timeout(Duration::from_secs(2), commands_two.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected replayed Read attempt");
        };
        assert_eq!(read_id_two, read_id_one);
        read_reply_two
            .send(Ok(ReadReply::Data(b"replayed-read".to_vec())))
            .unwrap();
        assert_eq!(
            read_call.await.unwrap().unwrap(),
            ReadReply::Data(b"replayed-read".to_vec())
        );

        let query_hub = hub.clone();
        let query_call = tokio::spawn(async move {
            query_hub
                .fs_query(
                    device_id,
                    FsQueryRequest::path_info("/replay/query").unwrap(),
                )
                .await
        });
        let RemoteCommand::FsQuery {
            request_id: query_id_one,
            reply: query_reply_one,
            ..
        } = commands_two.recv().await.unwrap()
        else {
            panic!("expected first FsQuery attempt");
        };
        drop(query_reply_one);
        hub.unregister(device_id, generation_two, Some(1_300));
        let (generation_three, mut commands_three) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Relay,
                1_400,
            )
            .unwrap();
        let RemoteCommand::FsQuery {
            request_id: query_id_two,
            reply: query_reply_two,
            ..
        } = tokio::time::timeout(Duration::from_secs(2), commands_three.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected replayed FsQuery attempt");
        };
        assert_eq!(query_id_two, query_id_one);
        let query_result = FsQueryReply::error(
            clew_transport::FsQueryErrorCode::NotFound,
            "replayed query proof",
        );
        query_reply_two.send(Ok(query_result.clone())).unwrap();
        assert_eq!(query_call.await.unwrap().unwrap(), query_result);

        let mutation_request = FsMutationRequest::write(
            "/replay/mutation",
            "replay mutation",
            clew_transport::FsWritePrecondition::CreateOnly,
        )
        .unwrap();
        let mutation_hub = hub.clone();
        let mutation_call =
            tokio::spawn(
                async move { mutation_hub.fs_mutation(device_id, mutation_request).await },
            );
        let RemoteCommand::FsMutation {
            request_id: mutation_id_one,
            reply: mutation_reply_one,
            ..
        } = commands_three.recv().await.unwrap()
        else {
            panic!("expected first FsMutation attempt");
        };
        mutation_reply_one
            .send(Ok(FsMutationReply::error(
                clew_transport::FsMutationErrorCode::Timeout,
                "still in progress",
            )))
            .unwrap();
        let RemoteCommand::FsMutation {
            request_id: mutation_id_two,
            reply: mutation_reply_two,
            ..
        } = tokio::time::timeout(Duration::from_secs(2), commands_three.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected same-generation FsMutation replay after in-flight timeout");
        };
        assert_eq!(mutation_id_two, mutation_id_one);
        drop(mutation_reply_two);
        hub.unregister(device_id, generation_three, Some(1_500));
        let (generation_four, mut commands_four) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_600,
            )
            .unwrap();
        let RemoteCommand::FsMutation {
            request_id: mutation_id_three,
            reply: mutation_reply_three,
            ..
        } = tokio::time::timeout(Duration::from_secs(2), commands_four.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected cross-generation FsMutation replay");
        };
        assert_eq!(mutation_id_three, mutation_id_one);
        let mutation_result = FsMutationReply::Result(clew_transport::FsMutationResult {
            sha256: "11".repeat(32),
            size: 15,
            created: true,
        });
        mutation_reply_three
            .send(Ok(mutation_result.clone()))
            .unwrap();
        assert_eq!(mutation_call.await.unwrap().unwrap(), mutation_result);

        let shell_hub = hub.clone();
        let shell_call = tokio::spawn(async move {
            shell_hub
                .shell_start(
                    device_id,
                    ShellTaskRequest::start("echo no-replay", "/replay", BTreeMap::new(), 5_000)
                        .unwrap(),
                )
                .await
        });
        let RemoteCommand::ShellTask { reply, .. } = commands_four.recv().await.unwrap() else {
            panic!("expected Shell Start on current generation");
        };
        drop(reply);
        hub.unregister(device_id, generation_four, Some(1_700));
        let (_generation_five, mut commands_five) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                1_800,
            )
            .unwrap();
        assert!(matches!(
            shell_call.await.unwrap(),
            Err(RemoteHubError::Offline(actual)) if actual == device_id
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), commands_five.recv())
                .await
                .is_err(),
            "Shell Start must not be replayed onto a new session generation"
        );
    }

    #[tokio::test]
    async fn tcp_forward_connection_is_generation_bound_and_never_replayed() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let forward_id = clew_core::ForwardId::new();
        let connection_id = clew_core::ForwardConnectionId::new();
        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_000,
            )
            .unwrap();

        let open_hub = hub.clone();
        let open = tokio::spawn(async move {
            open_hub
                .tcp_forward_open(
                    device_id,
                    TcpForwardRequest::open(
                        forward_id,
                        connection_id,
                        clew_transport::TcpForwardDestination::new("127.0.0.1", 8080).unwrap(),
                        1_000,
                    )
                    .unwrap(),
                )
                .await
        });
        let RemoteCommand::TcpForward { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected TCP forward Open on first generation");
        };
        assert!(matches!(
            request,
            TcpForwardRequest::Open { connection_id: actual, .. } if actual == connection_id
        ));
        reply
            .send(Ok(TcpForwardReply::Opened { connection_id }))
            .unwrap();
        let (opened_generation, opened_reply) = open.await.unwrap().unwrap();
        assert_eq!(opened_generation, generation_one);
        assert_eq!(opened_reply, TcpForwardReply::Opened { connection_id });

        let exchange_hub = hub.clone();
        let exchange = tokio::spawn(async move {
            exchange_hub
                .tcp_forward_on_generation(
                    device_id,
                    generation_one,
                    TcpForwardRequest::exchange(connection_id, b"ping", false, 4, 50).unwrap(),
                )
                .await
        });
        let RemoteCommand::TcpForward { reply, .. } = commands_one.recv().await.unwrap() else {
            panic!("expected TCP forward Exchange on original generation");
        };
        reply
            .send(Ok(TcpForwardReply::exchanged(
                connection_id,
                b"pong",
                false,
            )
            .unwrap()))
            .unwrap();
        let exchanged = exchange.await.unwrap().unwrap();
        assert_eq!(exchanged.read_bytes().unwrap(), b"pong");

        hub.unregister(device_id, generation_one, Some(1_100));
        let (_generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                1_200,
            )
            .unwrap();
        assert!(matches!(
            hub.tcp_forward_on_generation(
                device_id,
                generation_one,
                TcpForwardRequest::Close { connection_id },
            )
            .await,
            Err(RemoteHubError::TcpForwardSessionChanged(actual)) if actual == device_id
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), commands_two.recv())
                .await
                .is_err(),
            "established TCP forward must not replay Close/Exchange onto a new generation"
        );
    }
    #[tokio::test]
    async fn shell_projection_reattaches_to_same_device_after_status_proof() {
        let hub = RemoteHub::default();
        let device_a = DeviceId::new();
        let device_b = DeviceId::new();
        let base = unix_ms().unwrap();
        let (_generation_a, mut commands_a) = hub
            .register(
                device_a,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                base,
            )
            .unwrap();
        let (_generation_b, mut commands_b) = hub
            .register(
                device_b,
                RemoteSessionTopology::Direct,
                RemotePathState::Relay,
                base.saturating_add(1),
            )
            .unwrap();
        let task_id = TaskId::new();

        let start_hub = hub.clone();
        let start = tokio::spawn(async move {
            start_hub
                .shell_start(
                    device_a,
                    ShellTaskRequest::start(
                        "echo projection",
                        "/projection",
                        BTreeMap::new(),
                        5_000,
                    )
                    .unwrap(),
                )
                .await
        });
        let RemoteCommand::ShellTask { request, reply, .. } = commands_a.recv().await.unwrap()
        else {
            panic!("expected Shell Start on Device-A session");
        };
        assert!(matches!(request, ShellTaskRequest::Start { .. }));
        reply.send(Ok(ShellTaskReply::Started { task_id })).unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } = commands_a.recv().await.unwrap()
        else {
            panic!("expected automatic Shell Start receipt Status");
        };
        assert_eq!(request, ShellTaskRequest::Status { task_id });
        reply
            .send(Ok(ShellTaskReply::Status(
                clew_transport::ShellTaskStatus {
                    task_id,
                    phase: clew_transport::ShellTaskPhase::Running,
                    exit_code: None,
                    stdout_base_offset: 0,
                    stdout_next_offset: 0,
                    stderr_base_offset: 0,
                    stderr_next_offset: 0,
                },
            )))
            .unwrap();
        assert_eq!(
            start.await.unwrap().unwrap(),
            ShellTaskReply::Started { task_id }
        );
        assert_eq!(hub.shell_task_device(task_id).unwrap(), device_a);
        assert!(commands_b.try_recv().is_err());

        let mismatch_hub = hub.clone();
        let mismatch_call = tokio::spawn(async move {
            mismatch_hub
                .shell_task(ShellTaskRequest::Status { task_id })
                .await
        });
        let RemoteCommand::ShellTask { reply, .. } = commands_a.recv().await.unwrap() else {
            panic!("expected Shell Status on Device-A session");
        };
        reply
            .send(Ok(ShellTaskReply::Status(
                clew_transport::ShellTaskStatus {
                    task_id: TaskId::new(),
                    phase: clew_transport::ShellTaskPhase::Running,
                    exit_code: None,
                    stdout_base_offset: 0,
                    stdout_next_offset: 0,
                    stderr_base_offset: 0,
                    stderr_next_offset: 0,
                },
            )))
            .unwrap();
        assert!(matches!(
            mismatch_call.await.unwrap(),
            Err(RemoteHubError::ShellReplyTaskMismatch(actual)) if actual == task_id
        ));

        let (replacement_generation, mut replacement_commands) = hub
            .register(
                device_a,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                base.saturating_add(1_000),
            )
            .unwrap();
        assert_eq!(hub.shell_task_device(task_id).unwrap(), device_a);
        assert!(matches!(commands_a.recv().await, Some(RemoteCommand::Stop)));
        assert!(commands_b.try_recv().is_err());

        let reattach_hub = hub.clone();
        let reattach = tokio::spawn(async move {
            reattach_hub
                .shell_task(ShellTaskRequest::Status { task_id })
                .await
        });
        let RemoteCommand::ShellTask { request, reply, .. } =
            replacement_commands.recv().await.unwrap()
        else {
            panic!("expected reattach Status proof on replacement generation");
        };
        assert_eq!(request, ShellTaskRequest::Status { task_id });
        reply
            .send(Ok(ShellTaskReply::Status(
                clew_transport::ShellTaskStatus {
                    task_id,
                    phase: clew_transport::ShellTaskPhase::Running,
                    exit_code: None,
                    stdout_base_offset: 0,
                    stdout_next_offset: 0,
                    stderr_base_offset: 0,
                    stderr_next_offset: 0,
                },
            )))
            .unwrap();
        let (resolved_device, status_reply) = reattach.await.unwrap().unwrap();
        assert_eq!(resolved_device, device_a);
        assert!(
            matches!(status_reply, ShellTaskReply::Status(status) if status.task_id == task_id)
        );
        {
            let state = hub.inner.lock().unwrap();
            let projection = state.shell_tasks.get(&task_id).unwrap();
            assert_eq!(projection.device_id, device_a);
            assert_eq!(projection.generation, replacement_generation);
            assert_eq!(projection.reattach_deadline_unix_ms, None);
        }
        assert!(commands_b.try_recv().is_err());

        let cancel_hub = hub.clone();
        let cancel = tokio::spawn(async move {
            cancel_hub
                .shell_task(ShellTaskRequest::Cancel { task_id })
                .await
        });
        let RemoteCommand::ShellTask { request, reply, .. } =
            replacement_commands.recv().await.unwrap()
        else {
            panic!("expected Shell Cancel on reattached generation");
        };
        assert_eq!(request, ShellTaskRequest::Cancel { task_id });
        reply
            .send(Ok(ShellTaskReply::CancelAccepted { task_id }))
            .unwrap();
        let (resolved_device, cancel_reply) = cancel.await.unwrap().unwrap();
        assert_eq!(resolved_device, device_a);
        assert_eq!(cancel_reply, ShellTaskReply::CancelAccepted { task_id });
        assert!(commands_b.try_recv().is_err());
    }

    #[test]
    fn detached_shell_projection_expires_at_bounded_grace_deadline() {
        let task_id = TaskId::new();
        let device_id = DeviceId::new();
        let mut state = RemoteHubState::default();
        state.shell_tasks.insert(
            task_id,
            RemoteShellTaskProjection {
                device_id,
                generation: 7,
                reattach_deadline_unix_ms: Some(10_000),
            },
        );

        prune_expired_shell_projections(&mut state, 10_000);
        assert!(state.shell_tasks.contains_key(&task_id));
        prune_expired_shell_projections(&mut state, 10_001);
        assert!(!state.shell_tasks.contains_key(&task_id));
    }

    #[tokio::test]
    async fn aborted_shell_start_is_completed_and_orphan_task_is_cancelled() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let (_generation, mut commands) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                1_000,
            )
            .unwrap();
        let call_hub = hub.clone();
        let call = tokio::spawn(async move {
            call_hub
                .shell_start(
                    device_id,
                    ShellTaskRequest::start("echo pending", "/projection", BTreeMap::new(), 5_000)
                        .unwrap(),
                )
                .await
        });
        let RemoteCommand::ShellTask { request, reply, .. } = commands.recv().await.unwrap() else {
            panic!("expected pending Shell Start command");
        };
        assert!(matches!(request, ShellTaskRequest::Start { .. }));
        assert_eq!(hub.inner.lock().unwrap().pending_shell_starts, 1);
        call.abort();
        let _ = call.await;
        let task_id = TaskId::new();
        reply.send(Ok(ShellTaskReply::Started { task_id })).unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await
                .expect("orphan Shell task did not receive receipt Status")
                .unwrap()
        else {
            panic!("expected automatic Shell receipt Status command");
        };
        assert_eq!(request, ShellTaskRequest::Status { task_id });
        reply
            .send(Ok(ShellTaskReply::Status(
                clew_transport::ShellTaskStatus {
                    task_id,
                    phase: clew_transport::ShellTaskPhase::Running,
                    exit_code: None,
                    stdout_base_offset: 0,
                    stdout_next_offset: 0,
                    stderr_base_offset: 0,
                    stderr_next_offset: 0,
                },
            )))
            .unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await
                .expect("orphan Shell task was not cancelled after receipt proof")
                .unwrap()
        else {
            panic!("expected automatic Shell Cancel command");
        };
        assert_eq!(request, ShellTaskRequest::Cancel { task_id });
        reply
            .send(Ok(ShellTaskReply::CancelAccepted { task_id }))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let clean = {
                    let state = hub.inner.lock().unwrap();
                    state.pending_shell_starts == 0 && !state.shell_tasks.contains_key(&task_id)
                };
                if clean {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("orphan Shell projection was not cleaned up");
    }

    #[tokio::test]
    async fn lost_shell_start_receipt_reply_is_cancelled_after_reconnect() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let base = unix_ms().unwrap();
        let (generation_one, mut commands_one) = hub
            .register(
                device_id,
                RemoteSessionTopology::Direct,
                RemotePathState::Direct,
                base,
            )
            .unwrap();
        let call_hub = hub.clone();
        let call = tokio::spawn(async move {
            call_hub
                .shell_start(
                    device_id,
                    ShellTaskRequest::start(
                        "echo receipt-loss",
                        "/projection",
                        BTreeMap::new(),
                        5_000,
                    )
                    .unwrap(),
                )
                .await
        });

        let RemoteCommand::ShellTask { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected Shell Start before receipt-loss reconnect");
        };
        assert!(matches!(request, ShellTaskRequest::Start { .. }));
        let task_id = TaskId::new();
        reply.send(Ok(ShellTaskReply::Started { task_id })).unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } = commands_one.recv().await.unwrap()
        else {
            panic!("expected automatic receipt Status before simulated disconnect");
        };
        assert_eq!(request, ShellTaskRequest::Status { task_id });
        hub.unregister(device_id, generation_one, Some(base.saturating_add(10)));
        drop(reply);

        let (_generation_two, mut commands_two) = hub
            .register(
                device_id,
                RemoteSessionTopology::Connector,
                RemotePathState::MixedOrUnknown,
                base.saturating_add(20),
            )
            .unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands_two.recv())
                .await
                .expect("known orphan task did not re-prove itself after reconnect")
                .unwrap()
        else {
            panic!("expected reattach Status proof for known orphan task");
        };
        assert_eq!(request, ShellTaskRequest::Status { task_id });
        reply
            .send(Ok(ShellTaskReply::Status(
                clew_transport::ShellTaskStatus {
                    task_id,
                    phase: clew_transport::ShellTaskPhase::Running,
                    exit_code: None,
                    stdout_base_offset: 0,
                    stdout_next_offset: 0,
                    stderr_base_offset: 0,
                    stderr_next_offset: 0,
                },
            )))
            .unwrap();

        let RemoteCommand::ShellTask { request, reply, .. } =
            tokio::time::timeout(Duration::from_secs(2), commands_two.recv())
                .await
                .expect("known orphan task was not cancelled after reconnect proof")
                .unwrap()
        else {
            panic!("expected Cancel after known orphan reattach proof");
        };
        assert_eq!(request, ShellTaskRequest::Cancel { task_id });
        reply
            .send(Ok(ShellTaskReply::CancelAccepted { task_id }))
            .unwrap();

        assert!(matches!(
            call.await.unwrap(),
            Err(RemoteHubError::Offline(actual)) if actual == device_id
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !hub.inner.lock().unwrap().shell_tasks.contains_key(&task_id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("known orphan Shell projection was not cleaned after reconnect cancel");
    }

    #[test]
    fn closed_invite_control_error_becomes_safe_bootstrap_denial() {
        let error = RemoteConnectionError::Control(ControlStoreError::Enrollment(
            EnrollmentError::InviteClosed,
        ));
        let body = error
            .bootstrap_error_body()
            .expect("policy error is reportable");
        assert_eq!(body.code, BootstrapErrorCode::Denied);
        assert_eq!(body.message, "This invitation is closed to new devices.");
        assert_eq!(error.category(), "enrollment");
    }

    #[test]
    fn recovery_review_becomes_safe_bootstrap_denial() {
        let body = RemoteConnectionError::RecoveryReviewRequired
            .bootstrap_error_body()
            .expect("recovery review is reportable");
        assert_eq!(body.code, BootstrapErrorCode::Denied);
        assert!(body.message.contains("recovery review"));
    }
}
