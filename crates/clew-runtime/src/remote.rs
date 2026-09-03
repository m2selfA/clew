use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{DeviceId, DeviceNameOrigin, DeviceRecord, SiteId, TaskId};
use clew_host::normalize_hostname;
use clew_identity::{EnrollmentError, StoredControllerIdentity};
use clew_transport::{
    BootstrapErrorBody, BootstrapErrorCode, BootstrapMemberMode, BootstrapRequest,
    BootstrapResponse, ConnectorControlError, ConnectorLeaseError, ConnectorTunnelPurpose,
    ControllerSessionAuthority, FsMutationReply, FsMutationRequest, FsQueryReply, FsQueryRequest,
    HARD_MAX_SHELL_TASKS_PER_SESSION, InnerSession, IrohProtocol, IrohStream, ReadReply,
    ReadRequest, SealedBootstrapContext, SealedBootstrapError, SealedBootstrapSession,
    ShellTaskErrorCode, ShellTaskReply, ShellTaskRequest, SignedConnectorLease, SiteDiscoveryTag,
    read_bootstrap, read_connector_open, write_bootstrap,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{ControlStoreError, ControllerControlStore};

pub const MAX_REMOTE_CONNECTIONS: usize = 128;
const REMOTE_COMMAND_CAPACITY: usize = 16;
const MAX_REMOTE_SHELL_TASK_PROJECTIONS: usize =
    MAX_REMOTE_CONNECTIONS * HARD_MAX_SHELL_TASKS_PER_SESSION;
const CONNECTOR_LEASE_TTL_MS: u64 = 5 * 60 * 1000;
const SEALED_ACTIVATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default)]
pub struct RemoteHub {
    inner: Arc<Mutex<RemoteHubState>>,
}

#[derive(Debug, Default)]
struct RemoteHubState {
    next_token: u64,
    sessions: BTreeMap<DeviceId, RemoteSessionSlot>,
    shell_tasks: BTreeMap<TaskId, RemoteShellTaskProjection>,
    pending_shell_starts: usize,
}

#[derive(Debug)]
struct RemoteSessionSlot {
    token: u64,
    tx: mpsc::Sender<RemoteCommand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoteShellTaskProjection {
    device_id: DeviceId,
    token: u64,
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
        request: ReadRequest,
        reply: oneshot::Sender<Result<ReadReply, RemoteHubError>>,
    },
    FsQuery {
        request: FsQueryRequest,
        reply: oneshot::Sender<Result<FsQueryReply, RemoteHubError>>,
    },
    FsMutation {
        request: FsMutationRequest,
        reply: oneshot::Sender<Result<FsMutationReply, RemoteHubError>>,
    },
    ShellTask {
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

    pub async fn read(
        &self,
        device_id: DeviceId,
        request: ReadRequest,
    ) -> Result<ReadReply, RemoteHubError> {
        let tx = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .map(|slot| slot.tx.clone())
            .ok_or(RemoteHubError::Offline(device_id))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::Read {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;
        reply_rx
            .await
            .map_err(|_| RemoteHubError::Offline(device_id))?
    }

    pub async fn fs_query(
        &self,
        device_id: DeviceId,
        request: FsQueryRequest,
    ) -> Result<FsQueryReply, RemoteHubError> {
        let tx = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .map(|slot| slot.tx.clone())
            .ok_or(RemoteHubError::Offline(device_id))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::FsQuery {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;
        reply_rx
            .await
            .map_err(|_| RemoteHubError::Offline(device_id))?
    }

    pub async fn fs_mutation(
        &self,
        device_id: DeviceId,
        request: FsMutationRequest,
    ) -> Result<FsMutationReply, RemoteHubError> {
        let tx = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?
            .sessions
            .get(&device_id)
            .map(|slot| slot.tx.clone())
            .ok_or(RemoteHubError::Offline(device_id))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::FsMutation {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;
        reply_rx
            .await
            .map_err(|_| RemoteHubError::Offline(device_id))?
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
        let (token, tx) = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
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
            let token = slot.token;
            let tx = slot.tx.clone();
            state.pending_shell_starts += 1;
            (token, tx)
        };
        let reservation = ShellStartReservation::new(self.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::ShellTask {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;

        let hub = self.clone();
        let (completion_tx, completion_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut reservation = reservation;
            let completion = async {
                let reply = reply_rx
                    .await
                    .map_err(|_| RemoteHubError::Offline(device_id))??;
                reservation.release();
                let mut state = hub
                    .inner
                    .lock()
                    .map_err(|_| RemoteHubError::StatePoisoned)?;
                if !state
                    .sessions
                    .get(&device_id)
                    .is_some_and(|slot| slot.token == token)
                {
                    return Err(RemoteHubError::Offline(device_id));
                }
                match reply {
                    ShellTaskReply::Started { task_id } => {
                        if state.shell_tasks.contains_key(&task_id) {
                            return Err(RemoteHubError::ShellTaskIdConflict(task_id));
                        }
                        state
                            .shell_tasks
                            .insert(task_id, RemoteShellTaskProjection { device_id, token });
                        Ok(ShellTaskReply::Started { task_id })
                    }
                    ShellTaskReply::Error(error) => Ok(ShellTaskReply::Error(error)),
                    _ => Err(RemoteHubError::ShellReplyMismatch),
                }
            }
            .await;
            let orphan_task = match &completion {
                Ok(ShellTaskReply::Started { task_id }) => Some(*task_id),
                _ => None,
            };
            if completion_tx.send(completion).is_err()
                && let Some(task_id) = orphan_task
            {
                let _ = hub.shell_task(ShellTaskRequest::Cancel { task_id }).await;
                if let Ok(mut state) = hub.inner.lock()
                    && state.shell_tasks.get(&task_id).is_some_and(|projection| {
                        projection.device_id == device_id && projection.token == token
                    })
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
        let Some(projection) = state.shell_tasks.get(&task_id).copied() else {
            return Err(RemoteHubError::UnknownShellTask(task_id));
        };
        if !state
            .sessions
            .get(&projection.device_id)
            .is_some_and(|slot| slot.token == projection.token)
        {
            state.shell_tasks.remove(&task_id);
            return Err(RemoteHubError::UnknownShellTask(task_id));
        }
        Ok(projection.device_id)
    }

    pub async fn shell_task(
        &self,
        request: ShellTaskRequest,
    ) -> Result<(DeviceId, ShellTaskReply), RemoteHubError> {
        request.validate()?;
        let task_id =
            shell_request_task_id(&request).ok_or(RemoteHubError::InvalidShellProjectionRequest)?;
        let (device_id, token, tx) = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| RemoteHubError::StatePoisoned)?;
            let Some(projection) = state.shell_tasks.get(&task_id).copied() else {
                return Err(RemoteHubError::UnknownShellTask(task_id));
            };
            let Some(slot) = state.sessions.get(&projection.device_id) else {
                state.shell_tasks.remove(&task_id);
                return Err(RemoteHubError::UnknownShellTask(task_id));
            };
            if slot.token != projection.token {
                state.shell_tasks.remove(&task_id);
                return Err(RemoteHubError::UnknownShellTask(task_id));
            }
            (projection.device_id, projection.token, slot.tx.clone())
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(RemoteCommand::ShellTask {
            request: request.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| RemoteHubError::Offline(device_id))?;
        let reply = reply_rx
            .await
            .map_err(|_| RemoteHubError::Offline(device_id))??;
        validate_shell_reply_correlation(&request, &reply)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?;
        if !state
            .sessions
            .get(&device_id)
            .is_some_and(|slot| slot.token == token)
        {
            state.shell_tasks.remove(&task_id);
            return Err(RemoteHubError::UnknownShellTask(task_id));
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
        if let Ok(mut state) = self.inner.lock()
            && let Some(slot) = state.sessions.remove(&device_id)
        {
            state.shell_tasks.retain(|_, projection| {
                projection.device_id != device_id || projection.token != slot.token
            });
            let _ = slot.tx.try_send(RemoteCommand::Stop);
        }
    }

    fn register(
        &self,
        device_id: DeviceId,
    ) -> Result<(u64, mpsc::Receiver<RemoteCommand>), RemoteHubError> {
        let (tx, rx) = mpsc::channel(REMOTE_COMMAND_CAPACITY);
        let mut state = self
            .inner
            .lock()
            .map_err(|_| RemoteHubError::StatePoisoned)?;
        state.next_token = state
            .next_token
            .checked_add(1)
            .ok_or(RemoteHubError::TokenOverflow)?;
        let token = state.next_token;
        state
            .shell_tasks
            .retain(|_, projection| projection.device_id != device_id);
        if let Some(previous) = state
            .sessions
            .insert(device_id, RemoteSessionSlot { token, tx })
        {
            let _ = previous.tx.try_send(RemoteCommand::Stop);
        }
        Ok((token, rx))
    }

    fn unregister(&self, device_id: DeviceId, token: u64) {
        if let Ok(mut state) = self.inner.lock()
            && state
                .sessions
                .get(&device_id)
                .is_some_and(|slot| slot.token == token)
        {
            state.sessions.remove(&device_id);
            state.shell_tasks.retain(|_, projection| {
                projection.device_id != device_id || projection.token != token
            });
        }
    }
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
            handle_member(&mut stream, &identity, control, hub, None, true).await
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
            handle_member(stream, identity, control, hub, Some(site_tag), false).await
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
                        clew_identity::PermissionGrant::EXECUTE_READ_WRITE_SHELL_CONNECTOR
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

async fn handle_member(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
    hub: RemoteHub,
    expected_site_tag: Option<SiteDiscoveryTag>,
    issue_connector_lease: bool,
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
    let (token, mut commands) = hub.register(device_id)?;
    while let Some(command) = commands.recv().await {
        match command {
            RemoteCommand::Read { request, reply } => {
                let result = async {
                    inner.send(stream, &request.into_message()?).await?;
                    let message = inner.recv(stream).await?;
                    Ok(ReadReply::from_message(&message)?)
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::FsQuery { request, reply } => {
                let result = async {
                    inner.send(stream, &request.into_message()?).await?;
                    let message = inner.recv(stream).await?;
                    Ok(FsQueryReply::from_message(&message)?)
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::FsMutation { request, reply } => {
                let result = async {
                    inner.send(stream, &request.into_message()?).await?;
                    let message = inner.recv(stream).await?;
                    Ok(FsMutationReply::from_message(&message)?)
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::ShellTask { request, reply } => {
                let result = async {
                    inner.send(stream, &request.into_message()?).await?;
                    let message = inner.recv(stream).await?;
                    Ok(ShellTaskReply::from_message(&message)?)
                }
                .await;
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            RemoteCommand::Stop => break,
        }
    }
    hub.unregister(device_id, token);
    Ok(())
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
    #[error("remote session token overflow")]
    TokenOverflow,
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
    Inner(#[from] clew_transport::InnerSessionError),
    #[error(transparent)]
    FsQuery(#[from] clew_transport::FsQueryProtocolError),
    #[error(transparent)]
    FsMutation(#[from] clew_transport::FsMutationProtocolError),
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
    use super::*;

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

    #[tokio::test]
    async fn shell_projection_binds_task_to_live_device_session_and_reconnect_invalidates_it() {
        let hub = RemoteHub::default();
        let device_a = DeviceId::new();
        let device_b = DeviceId::new();
        let (_token_a, mut commands_a) = hub.register(device_a).unwrap();
        let (_token_b, mut commands_b) = hub.register(device_b).unwrap();
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
        let RemoteCommand::ShellTask { request, reply } = commands_a.recv().await.unwrap() else {
            panic!("expected Shell Start on Device-A session");
        };
        assert!(matches!(request, ShellTaskRequest::Start { .. }));
        reply.send(Ok(ShellTaskReply::Started { task_id })).unwrap();
        assert_eq!(
            start.await.unwrap().unwrap(),
            ShellTaskReply::Started { task_id }
        );
        assert_eq!(hub.shell_task_device(task_id).unwrap(), device_a);
        assert!(commands_b.try_recv().is_err());

        let status_hub = hub.clone();
        let status_call = tokio::spawn(async move {
            status_hub
                .shell_task(ShellTaskRequest::Status { task_id })
                .await
        });
        let RemoteCommand::ShellTask { request, reply } = commands_a.recv().await.unwrap() else {
            panic!("expected Shell Status on Device-A session");
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
        let (resolved_device, status_reply) = status_call.await.unwrap().unwrap();
        assert_eq!(resolved_device, device_a);
        assert!(
            matches!(status_reply, ShellTaskReply::Status(status) if status.task_id == task_id)
        );
        assert!(commands_b.try_recv().is_err());

        let mismatch_hub = hub.clone();
        let mismatch_call = tokio::spawn(async move {
            mismatch_hub
                .shell_task(ShellTaskRequest::Status { task_id })
                .await
        });
        let RemoteCommand::ShellTask { reply, .. } = commands_a.recv().await.unwrap() else {
            panic!("expected second Shell Status on Device-A session");
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

        let (_replacement_token, _replacement_commands) = hub.register(device_a).unwrap();
        assert!(matches!(
            hub.shell_task_device(task_id),
            Err(RemoteHubError::UnknownShellTask(actual)) if actual == task_id
        ));
        assert!(matches!(commands_a.recv().await, Some(RemoteCommand::Stop)));
    }

    #[tokio::test]
    async fn aborted_shell_start_is_completed_and_orphan_task_is_cancelled() {
        let hub = RemoteHub::default();
        let device_id = DeviceId::new();
        let (_token, mut commands) = hub.register(device_id).unwrap();
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
        let RemoteCommand::ShellTask { request, reply } = commands.recv().await.unwrap() else {
            panic!("expected pending Shell Start command");
        };
        assert!(matches!(request, ShellTaskRequest::Start { .. }));
        assert_eq!(hub.inner.lock().unwrap().pending_shell_starts, 1);
        call.abort();
        let _ = call.await;
        let task_id = TaskId::new();
        reply.send(Ok(ShellTaskReply::Started { task_id })).unwrap();

        let RemoteCommand::ShellTask { request, reply } =
            tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await
                .expect("orphan Shell task was not cancelled")
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
