use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    time::Duration,
};

use clew_core::{DeviceId, DeviceRecord, RequestId, SiteId, StateLayout};
use clew_identity::{ControllerPublicIdentity, DeviceIdentityStore};
use clew_transport::{
    BootstrapErrorCode, BootstrapMemberMode, BootstrapRequest, BootstrapResponse,
    ConnectorControlError, ConnectorDiscoveryError, ConnectorDiscoveryEvent, ConnectorLeaseError,
    ConnectorOpenRequest, ConnectorReady, ConnectorTunnelPurpose, DeviceSessionIdentity,
    FileTransferErrorCode, FileTransferReply, FileTransferRequest, FsMutationErrorCode,
    FsMutationReply, FsMutationRequest, FsQueryErrorCode, FsQueryReply, FsQueryRequest,
    InnerMessage, InnerSession, IrohOuter, IrohProtocol, MdnsConnectorDiscovery,
    NearbyConnectorFile, RPC_PROGRESS_INTERVAL_MS, ReadErrorCode, ReadReply, ReadRequest,
    SealedBootstrapContext, SealedBootstrapError, SealedBootstrapSession, ShellTaskErrorCode,
    ShellTaskReply, ShellTaskRequest, SignedConnectorLease, SiteDiscoveryTag, TcpForwardErrorCode,
    TcpForwardReply, TcpForwardRequest, forward_opaque_bidirectional, read_bootstrap,
    read_connector_open, read_connector_ready, unwrap_rpc_request, wrap_rpc_progress,
    wrap_rpc_reply, write_bootstrap, write_connector_open, write_connector_ready,
};
use iroh::{EndpointAddr, EndpointId};
use thiserror::Error;
use tokio::{sync::watch, task::JoinSet, time::Instant};

use crate::{
    HostFileTransferService, HostLaunchState, HostMembership, HostMembershipStore, HostReadService,
    HostShellService, HostTcpForwardService, NearbyConnectorStore,
};

const MAX_CONNECTOR_TUNNELS: usize = 64;
const CONNECTOR_LEASE_RENEW_MARGIN: Duration = Duration::from_secs(30);
const INITIAL_BOOTSTRAP_PATH_WINDOW: Duration = Duration::from_secs(20);
const ACTIVE_MEMBER_PATH_WINDOW: Duration = Duration::from_secs(20);
const MAX_CONCURRENT_CONNECTOR_DIALS: usize = 4;
const MAX_CONNECTOR_CANDIDATES_PER_WINDOW: usize = 32;
const CONNECTOR_CANDIDATE_DIAL_TIMEOUT: Duration = Duration::from_secs(12);
const CONNECTOR_PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub async fn complete_networked_activation(
    layout: &StateLayout,
    state: HostLaunchState,
) -> Result<HostLaunchState, HostRemoteError> {
    complete_networked_activation_with_window(layout, state, INITIAL_BOOTSTRAP_PATH_WINDOW).await
}

async fn complete_networked_activation_with_window(
    layout: &StateLayout,
    state: HostLaunchState,
    path_window: Duration,
) -> Result<HostLaunchState, HostRemoteError> {
    match state {
        HostLaunchState::AwaitingEnrollment {
            site_file,
            pending,
            hostname,
            launch_mode,
            source,
        } => {
            let controller = site_file.payload.bootstrap.payload.controller;
            let site_id = site_file.payload.bootstrap.payload.site_id;
            if let Some(membership) =
                HostMembershipStore::new(layout.clone()).load(controller.controller_id, site_id)?
            {
                resume_pending_controller_activation_with_window(layout, &membership, path_window)
                    .await?;
                return Ok(HostLaunchState::Active { membership, source });
            }
            let endpoint = site_file.payload.controller_endpoint.clone();
            let read_policy = site_file.payload.read_policy.clone();
            let (endpoint, read_policy) = match (endpoint, read_policy) {
                (Some(endpoint), Some(read_policy)) => (endpoint, read_policy),
                (None, None) => {
                    return Ok(HostLaunchState::AwaitingEnrollment {
                        site_file,
                        pending,
                        hostname,
                        launch_mode,
                        source,
                    });
                }
                _ => return Err(HostRemoteError::MissingNetworkConfig),
            };
            let outer = IrohOuter::bind().await?;
            let mut channel = connect_bootstrap_channel(
                layout,
                &outer,
                endpoint.clone(),
                controller,
                site_id,
                site_file.payload.controller_bootstrap_noise_public_key,
                path_window,
            )
            .await?;
            channel
                .send(&BootstrapRequest::Claim {
                    bootstrap: site_file.payload.bootstrap.clone(),
                    device_identity: pending.public_identity(),
                    hostname: hostname.clone(),
                    mode: bootstrap_member_mode(site_file.payload.role_hint, launch_mode),
                })
                .await?;
            let receipt = match channel.recv::<BootstrapResponse>().await? {
                BootstrapResponse::Claimed(receipt) => receipt,
                BootstrapResponse::Error(error) => {
                    return Err(HostRemoteError::BootstrapRejected {
                        code: error.code,
                        message: error.message,
                    });
                }
                BootstrapResponse::Activated(_) | BootstrapResponse::ActivationConfirmed { .. } => {
                    return Err(HostRemoteError::UnexpectedBootstrapResponse);
                }
            };
            let membership = HostMembershipStore::new(layout.clone()).activate_networked(
                site_file.payload.client_flavor.clone(),
                site_file.payload.outfit_profile.clone(),
                &site_file.payload.bootstrap.payload.site_name,
                &pending,
                &receipt,
                &hostname,
                endpoint,
                read_policy,
                site_file.payload.controller_bootstrap_noise_public_key,
            )?;
            channel
                .send(&BootstrapRequest::Persisted {
                    invite_id: receipt.invite_id,
                    device_id: receipt.device_id,
                    persist_ack_token: *receipt.persist_ack_token(),
                    hostname: hostname.clone(),
                })
                .await?;
            let activated = expect_activated(channel.recv().await?)?;
            verify_activated(&membership, &activated)?;
            channel
                .send(&BootstrapRequest::ActivatedAck {
                    invite_id: receipt.invite_id,
                    device_id: receipt.device_id,
                })
                .await?;
            confirm_sealed_activation(&mut channel, receipt.invite_id, receipt.device_id).await?;
            DeviceIdentityStore::new(layout.clone()).confirm_controller_activation(
                membership.marker.controller.controller_id,
                membership.marker.site_id,
                membership.marker.device_id,
            )?;
            Ok(HostLaunchState::Active { membership, source })
        }
        HostLaunchState::Active { membership, source } => {
            resume_pending_controller_activation_with_window(layout, &membership, path_window)
                .await?;
            Ok(HostLaunchState::Active { membership, source })
        }
        other => Ok(other),
    }
}

fn bootstrap_member_mode(
    signed_role: crate::HostRoleHint,
    launch_mode: crate::HostLaunchMode,
) -> BootstrapMemberMode {
    if signed_role == crate::HostRoleHint::ConnectorOnly
        || launch_mode == crate::HostLaunchMode::ConnectorOnly
    {
        BootstrapMemberMode::ConnectorOnly
    } else {
        BootstrapMemberMode::ExecutePreferred
    }
}

async fn resume_pending_controller_activation_with_window(
    layout: &StateLayout,
    membership: &HostMembership,
    path_window: Duration,
) -> Result<(), HostRemoteError> {
    let identity_store = DeviceIdentityStore::new(layout.clone());
    let Some(activation) = identity_store.load_pending_controller_activation(
        membership.marker.controller.controller_id,
        membership.marker.site_id,
    )?
    else {
        return Ok(());
    };
    if activation.device_id() != membership.marker.device_id
        || activation.invite_id() != membership.marker.invite_id
    {
        return Err(HostRemoteError::ActivationScopeMismatch);
    }
    let endpoint = membership
        .marker
        .controller_endpoint
        .clone()
        .ok_or(HostRemoteError::MissingNetworkConfig)?;
    let outer = IrohOuter::bind().await?;
    let mut channel = connect_bootstrap_channel(
        layout,
        &outer,
        endpoint,
        membership.marker.controller,
        membership.marker.site_id,
        membership.marker.controller_bootstrap_noise_public_key,
        path_window,
    )
    .await?;
    channel
        .send(&BootstrapRequest::Persisted {
            invite_id: activation.invite_id(),
            device_id: activation.device_id(),
            persist_ack_token: *activation.persist_ack_token(),
            hostname: membership.device.hostname_observed.clone(),
        })
        .await?;
    let activated = expect_activated(channel.recv().await?)?;
    verify_activated(membership, &activated)?;
    channel
        .send(&BootstrapRequest::ActivatedAck {
            invite_id: activation.invite_id(),
            device_id: activation.device_id(),
        })
        .await?;
    confirm_sealed_activation(&mut channel, activation.invite_id(), activation.device_id()).await?;
    identity_store.confirm_controller_activation(
        membership.marker.controller.controller_id,
        membership.marker.site_id,
        membership.marker.device_id,
    )?;
    Ok(())
}

pub async fn wait_for_networked_activation_until(
    layout: &StateLayout,
    state: HostLaunchState,
    shutdown: watch::Receiver<bool>,
) -> Result<Option<HostLaunchState>, HostRemoteError> {
    wait_for_networked_activation_until_with_timing(
        layout,
        state,
        shutdown,
        INITIAL_BOOTSTRAP_PATH_WINDOW,
        Duration::from_secs(1),
    )
    .await
}

async fn wait_for_networked_activation_until_with_timing(
    layout: &StateLayout,
    mut state: HostLaunchState,
    mut shutdown: watch::Receiver<bool>,
    path_window: Duration,
    retry_delay: Duration,
) -> Result<Option<HostLaunchState>, HostRemoteError> {
    loop {
        if *shutdown.borrow() {
            return Ok(None);
        }
        let attempt = complete_networked_activation_with_window(layout, state.clone(), path_window);
        tokio::pin!(attempt);
        let result = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
                continue;
            }
            result = &mut attempt => result,
        };
        match result {
            Ok(next @ HostLaunchState::Active { .. }) => return Ok(Some(next)),
            Ok(next @ HostLaunchState::MissingInvite { .. })
            | Ok(next @ HostLaunchState::AmbiguousMembership { .. }) => return Ok(Some(next)),
            Ok(next @ HostLaunchState::AwaitingEnrollment { .. }) => state = next,
            Err(error) if error.is_retryable_activation() => {}
            Err(error) => return Err(error),
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
            _ = tokio::time::sleep(retry_delay) => {}
        }
    }
}

pub async fn serve_networked_membership_until<F>(
    membership: &HostMembership,
    shutdown: F,
) -> Result<(), HostRemoteError>
where
    F: Future<Output = ()>,
{
    serve_networked_membership_until_inner(membership, None, shutdown).await
}

pub async fn serve_networked_membership_until_with_layout<F>(
    layout: &StateLayout,
    membership: &HostMembership,
    shutdown: F,
) -> Result<(), HostRemoteError>
where
    F: Future<Output = ()>,
{
    serve_networked_membership_until_inner(membership, Some(layout), shutdown).await
}

async fn serve_networked_membership_until_inner<F>(
    membership: &HostMembership,
    layout: Option<&StateLayout>,
    shutdown: F,
) -> Result<(), HostRemoteError>
where
    F: Future<Output = ()>,
{
    let (endpoint, service, shell_service, file_transfer_service) =
        member_remote_config(membership)?;
    tokio::pin!(shutdown);
    let outer = tokio::select! {
        _ = &mut shutdown => return Ok(()),
        result = IrohOuter::bind() => result?,
    };
    loop {
        let result = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = serve_networked_membership_with_outer(
                membership,
                &outer,
                endpoint.clone(),
                &service,
                &shell_service,
                &file_transfer_service,
                layout,
            ) => result,
        };
        if matches!(
            result,
            Err(HostRemoteError::MissingNetworkConfig | HostRemoteError::ExecutionDisabled)
        ) {
            return result;
        }
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

pub async fn serve_networked_membership_once(
    membership: &HostMembership,
) -> Result<(), HostRemoteError> {
    let (endpoint, service, shell_service, file_transfer_service) =
        member_remote_config(membership)?;
    let outer = IrohOuter::bind().await?;
    serve_networked_membership_with_outer(
        membership,
        &outer,
        endpoint,
        &service,
        &shell_service,
        &file_transfer_service,
        None,
    )
    .await
}

pub async fn serve_networked_membership_once_with_layout(
    layout: &StateLayout,
    membership: &HostMembership,
) -> Result<(), HostRemoteError> {
    let (endpoint, service, shell_service, file_transfer_service) =
        member_remote_config(membership)?;
    let outer = IrohOuter::bind().await?;
    serve_networked_membership_with_outer(
        membership,
        &outer,
        endpoint,
        &service,
        &shell_service,
        &file_transfer_service,
        Some(layout),
    )
    .await
}

fn member_remote_config(
    membership: &HostMembership,
) -> Result<
    (
        EndpointAddr,
        Option<HostReadService>,
        Option<HostShellService>,
        Option<HostFileTransferService>,
    ),
    HostRemoteError,
> {
    if !membership.device.capabilities.execute && !membership.device.capabilities.connector {
        return Err(HostRemoteError::ExecutionDisabled);
    }
    let endpoint = membership
        .marker
        .controller_endpoint
        .clone()
        .ok_or(HostRemoteError::MissingNetworkConfig)?;
    let (service, shell_service, file_transfer_service) = if membership.device.capabilities.execute
    {
        let policy = membership
            .marker
            .read_policy
            .clone()
            .ok_or(HostRemoteError::MissingNetworkConfig)?;
        let grant = membership.marker.effective_grant.as_ref();
        let shell_service = grant
            .is_some_and(|grant| grant.shell)
            .then(|| HostShellService::new(policy.clone()))
            .transpose()?;
        let can_get = grant.is_some_and(|grant| grant.read);
        let can_put = grant.is_some_and(|grant| grant.write);
        let file_transfer_service = (can_get || can_put)
            .then(|| {
                HostFileTransferService::new(
                    policy.clone(),
                    membership.marker.controller.controller_id,
                    membership.marker.site_id,
                    membership.marker.device_id,
                    can_get,
                    can_put,
                )
            })
            .transpose()?;
        (
            Some(HostReadService::new(policy)?),
            shell_service,
            file_transfer_service,
        )
    } else {
        (None, None, None)
    };
    Ok((endpoint, service, shell_service, file_transfer_service))
}

enum HostBootstrapChannel {
    Direct(clew_transport::IrohStream),
    Sealed {
        stream: clew_transport::IrohStream,
        session: SealedBootstrapSession,
    },
}

impl HostBootstrapChannel {
    async fn send<T: serde::Serialize>(&mut self, value: &T) -> Result<(), HostRemoteError> {
        match self {
            Self::Direct(stream) => write_bootstrap(stream, value).await?,
            Self::Sealed { stream, session } => session.send(stream, value).await?,
        }
        Ok(())
    }

    async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, HostRemoteError> {
        Ok(match self {
            Self::Direct(stream) => read_bootstrap(stream).await?,
            Self::Sealed { stream, session } => session.recv(stream).await?,
        })
    }
    fn is_sealed(&self) -> bool {
        matches!(self, Self::Sealed { .. })
    }
}

async fn confirm_sealed_activation(
    channel: &mut HostBootstrapChannel,
    invite_id: clew_core::InviteId,
    device_id: DeviceId,
) -> Result<(), HostRemoteError> {
    if !channel.is_sealed() {
        return Ok(());
    }
    match channel.recv::<BootstrapResponse>().await? {
        BootstrapResponse::ActivationConfirmed {
            invite_id: actual_invite,
            device_id: actual_device,
        } if actual_invite == invite_id && actual_device == device_id => {
            channel
                .send(&BootstrapRequest::ActivationConfirmedAck {
                    invite_id,
                    device_id,
                })
                .await?;
            Ok(())
        }
        BootstrapResponse::Error(error) => Err(HostRemoteError::BootstrapRejected {
            code: error.code,
            message: error.message,
        }),
        _ => Err(HostRemoteError::UnexpectedBootstrapResponse),
    }
}

#[derive(Default)]
struct ConnectorCandidateQueue {
    seen: BTreeSet<EndpointId>,
    pending: VecDeque<EndpointAddr>,
}

impl ConnectorCandidateQueue {
    fn push(&mut self, candidate: EndpointAddr) {
        if self.seen.len() >= MAX_CONNECTOR_CANDIDATES_PER_WINDOW || !self.seen.insert(candidate.id)
        {
            return;
        }
        self.pending.push_back(candidate);
    }

    fn pop(&mut self) -> Option<EndpointAddr> {
        self.pending.pop_front()
    }
}

fn fill_bootstrap_candidate_dials(
    tasks: &mut JoinSet<Result<HostBootstrapChannel, HostRemoteError>>,
    queue: &mut ConnectorCandidateQueue,
    outer: &IrohOuter,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    bootstrap_key: [u8; 32],
) {
    while tasks.len() < MAX_CONCURRENT_CONNECTOR_DIALS {
        let Some(candidate) = queue.pop() else {
            break;
        };
        let outer = outer.clone();
        tasks.spawn(async move {
            tokio::time::timeout(
                CONNECTOR_CANDIDATE_DIAL_TIMEOUT,
                connect_bootstrap_via_candidate(
                    &outer,
                    candidate,
                    controller,
                    site_id,
                    bootstrap_key,
                ),
            )
            .await
            .map_err(|_| HostRemoteError::ConnectorCandidateTimeout)?
        });
    }
}

fn fill_member_candidate_dials(
    tasks: &mut JoinSet<Result<clew_transport::IrohStream, HostRemoteError>>,
    queue: &mut ConnectorCandidateQueue,
    outer: &IrohOuter,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
) {
    while tasks.len() < MAX_CONCURRENT_CONNECTOR_DIALS {
        let Some(candidate) = queue.pop() else {
            break;
        };
        let outer = outer.clone();
        tasks.spawn(async move {
            tokio::time::timeout(
                CONNECTOR_CANDIDATE_DIAL_TIMEOUT,
                connect_member_via_candidate(&outer, candidate, controller, site_id),
            )
            .await
            .map_err(|_| HostRemoteError::ConnectorCandidateTimeout)?
        });
    }
}

async fn connect_bootstrap_channel(
    layout: &StateLayout,
    outer: &IrohOuter,
    controller_endpoint: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    controller_bootstrap_noise_public_key: Option<[u8; 32]>,
    path_window: Duration,
) -> Result<HostBootstrapChannel, HostRemoteError> {
    let mut direct_tasks = JoinSet::new();
    let direct_outer = outer.clone();
    direct_tasks.spawn(async move {
        direct_outer
            .connect_bootstrap(controller_endpoint)
            .await
            .map_err(HostRemoteError::from)
    });

    let Some(bootstrap_key) = controller_bootstrap_noise_public_key else {
        return match tokio::time::timeout(path_window, direct_tasks.join_next()).await {
            Ok(Some(Ok(Ok(stream)))) => Ok(HostBootstrapChannel::Direct(stream)),
            Ok(Some(Ok(Err(error)))) => Err(error),
            Ok(Some(Err(error))) => Err(HostRemoteError::ConnectorTask(error.to_string())),
            Ok(None) | Err(_) => Err(HostRemoteError::BootstrapPathUnavailable),
        };
    };

    let discovery =
        MdnsConnectorDiscovery::attach(outer, controller.controller_id, site_id, false)?;
    let mut events = discovery.subscribe().await;
    let mut candidates = ConnectorCandidateQueue::default();
    if let Some(candidate) = NearbyConnectorStore::new(layout.clone())
        .load_import(&controller, site_id)
        .ok()
        .flatten()
        .map(|file| file.connector_candidate().addr)
    {
        candidates.push(candidate);
    }
    let mut candidate_tasks = JoinSet::new();
    fill_bootstrap_candidate_dials(
        &mut candidate_tasks,
        &mut candidates,
        outer,
        controller,
        site_id,
        bootstrap_key,
    );
    let deadline = tokio::time::sleep(path_window);
    tokio::pin!(deadline);
    let mut direct_done = false;

    loop {
        tokio::select! {
            joined = direct_tasks.join_next(), if !direct_done => {
                match joined {
                    Some(Ok(Ok(stream))) => return Ok(HostBootstrapChannel::Direct(stream)),
                    Some(Ok(Err(_))) | None => direct_done = true,
                    Some(Err(error)) => {
                        return Err(HostRemoteError::ConnectorTask(error.to_string()));
                    }
                }
            }
            joined = candidate_tasks.join_next(), if !candidate_tasks.is_empty() => {
                match joined {
                    Some(Ok(Ok(channel))) => return Ok(channel),
                    Some(Ok(Err(_))) | None => {
                        fill_bootstrap_candidate_dials(
                            &mut candidate_tasks,
                            &mut candidates,
                            outer,
                            controller,
                            site_id,
                            bootstrap_key,
                        );
                    }
                    Some(Err(error)) => {
                        return Err(HostRemoteError::ConnectorTask(error.to_string()));
                    }
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    continue;
                };
                let ConnectorDiscoveryEvent::Candidate(candidate) = event else {
                    continue;
                };
                candidates.push(candidate.addr);
                fill_bootstrap_candidate_dials(
                    &mut candidate_tasks,
                    &mut candidates,
                    outer,
                    controller,
                    site_id,
                    bootstrap_key,
                );
            }
            _ = &mut deadline => return Err(HostRemoteError::BootstrapPathUnavailable),
        }
    }
}

async fn connect_bootstrap_via_candidate(
    outer: &IrohOuter,
    candidate: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    controller_bootstrap_noise_public_key: [u8; 32],
) -> Result<HostBootstrapChannel, HostRemoteError> {
    let mut stream = outer.connect_connector(candidate.clone()).await?;
    let open = ConnectorOpenRequest::new(
        SiteDiscoveryTag::derive(controller.controller_id, site_id),
        ConnectorTunnelPurpose::Bootstrap,
    );
    write_connector_open(&mut stream, &open).await?;
    let ready = read_connector_ready(&mut stream).await?;
    ready
        .lease
        .verify_for_candidate(&controller, site_id, candidate.id, unix_ms()?)?;
    let session = SealedBootstrapSession::connect(
        &mut stream,
        SealedBootstrapContext {
            controller_id: controller.controller_id,
            site_id,
        },
        controller_bootstrap_noise_public_key,
    )
    .await?;
    Ok(HostBootstrapChannel::Sealed { stream, session })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemberOuterPath {
    Direct,
    Connector,
}

async fn connect_member_stream(
    layout: Option<&StateLayout>,
    outer: &IrohOuter,
    controller_endpoint: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
) -> Result<(clew_transport::IrohStream, MemberOuterPath), HostRemoteError> {
    let mut direct_tasks = JoinSet::new();
    let direct_outer = outer.clone();
    direct_tasks.spawn(async move {
        direct_outer
            .connect(controller_endpoint)
            .await
            .map_err(HostRemoteError::from)
    });
    let discovery =
        MdnsConnectorDiscovery::attach(outer, controller.controller_id, site_id, false)?;
    let mut events = discovery.subscribe().await;
    let mut candidates = ConnectorCandidateQueue::default();
    if let Some(candidate) = layout
        .and_then(|layout| {
            NearbyConnectorStore::new(layout.clone())
                .load_import(&controller, site_id)
                .ok()
                .flatten()
        })
        .map(|file| file.connector_candidate().addr)
    {
        candidates.push(candidate);
    }
    let mut candidate_tasks = JoinSet::new();
    fill_member_candidate_dials(
        &mut candidate_tasks,
        &mut candidates,
        outer,
        controller,
        site_id,
    );
    let deadline = tokio::time::sleep(ACTIVE_MEMBER_PATH_WINDOW);
    tokio::pin!(deadline);
    let mut direct_done = false;

    loop {
        tokio::select! {
            joined = direct_tasks.join_next(), if !direct_done => {
                match joined {
                    Some(Ok(Ok(stream))) => return Ok((stream, MemberOuterPath::Direct)),
                    Some(Ok(Err(_))) | None => direct_done = true,
                    Some(Err(error)) => {
                        return Err(HostRemoteError::ConnectorTask(error.to_string()));
                    }
                }
            }
            joined = candidate_tasks.join_next(), if !candidate_tasks.is_empty() => {
                match joined {
                    Some(Ok(Ok(stream))) => return Ok((stream, MemberOuterPath::Connector)),
                    Some(Ok(Err(_))) | None => {
                        fill_member_candidate_dials(
                            &mut candidate_tasks,
                            &mut candidates,
                            outer,
                            controller,
                            site_id,
                        );
                    }
                    Some(Err(error)) => {
                        return Err(HostRemoteError::ConnectorTask(error.to_string()));
                    }
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    continue;
                };
                let ConnectorDiscoveryEvent::Candidate(candidate) = event else {
                    continue;
                };
                candidates.push(candidate.addr);
                fill_member_candidate_dials(
                    &mut candidate_tasks,
                    &mut candidates,
                    outer,
                    controller,
                    site_id,
                );
            }
            _ = &mut deadline => return Err(HostRemoteError::MemberPathUnavailable),
        }
    }
}

async fn connect_member_via_candidate(
    outer: &IrohOuter,
    candidate: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
) -> Result<clew_transport::IrohStream, HostRemoteError> {
    let mut stream = outer.connect_connector(candidate.clone()).await?;
    let open = ConnectorOpenRequest::new(
        SiteDiscoveryTag::derive(controller.controller_id, site_id),
        ConnectorTunnelPurpose::InnerSession,
    );
    write_connector_open(&mut stream, &open).await?;
    let ready = read_connector_ready(&mut stream).await?;
    ready
        .lease
        .verify_for_candidate(&controller, site_id, candidate.id, unix_ms()?)?;
    Ok(stream)
}

async fn serve_networked_membership_with_outer(
    membership: &HostMembership,
    outer: &IrohOuter,
    endpoint: EndpointAddr,
    service: &Option<HostReadService>,
    shell_service: &Option<HostShellService>,
    file_transfer_service: &Option<HostFileTransferService>,
    layout: Option<&StateLayout>,
) -> Result<(), HostRemoteError> {
    serve_networked_membership_with_outer_timing(
        membership,
        outer,
        endpoint,
        service,
        shell_service,
        file_transfer_service,
        layout,
        CONNECTOR_PRESENCE_REFRESH_INTERVAL,
    )
    .await
}

async fn complete_rpc_with_progress<F>(
    inner: &mut InnerSession,
    stream: &mut clew_transport::IrohStream,
    request_id: RequestId,
    operation: F,
) -> Result<InnerMessage, HostRemoteError>
where
    F: Future<Output = Result<InnerMessage, HostRemoteError>>,
{
    tokio::pin!(operation);
    let interval = Duration::from_millis(RPC_PROGRESS_INTERVAL_MS);
    let mut progress = tokio::time::interval_at(Instant::now() + interval, interval);
    progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = progress.tick() => {
                let progress = wrap_rpc_progress(request_id)?;
                inner.send(stream, &progress).await?;
            }
        }
    }
}

async fn serve_networked_membership_with_outer_timing(
    membership: &HostMembership,
    outer: &IrohOuter,
    endpoint: EndpointAddr,
    service: &Option<HostReadService>,
    shell_service: &Option<HostShellService>,
    file_transfer_service: &Option<HostFileTransferService>,
    layout: Option<&StateLayout>,
    presence_refresh_interval: Duration,
) -> Result<(), HostRemoteError> {
    let (mut stream, outer_path) = connect_member_stream(
        layout,
        outer,
        endpoint.clone(),
        membership.marker.controller,
        membership.marker.site_id,
    )
    .await?;
    let mut inner = InnerSession::connect(
        &mut stream,
        DeviceSessionIdentity::from_active(&membership.identity),
    )
    .await?;
    let _shell_session = shell_service.as_ref().map(HostShellService::attach_session);

    let connector =
        membership.device.capabilities.connector && outer_path == MemberOuterPath::Direct;
    let mut connector_discovery = None;
    let mut connector_lease = None;
    let mut presence_refresh_at = Instant::now() + Duration::from_secs(24 * 60 * 60);
    let renew_at = if connector {
        let message = inner.recv(&mut stream).await?;
        let lease = SignedConnectorLease::from_message(&message)?;
        let now = unix_ms()?;
        let lease_device = lease.verify_for_candidate(
            &membership.marker.controller,
            membership.marker.site_id,
            outer.addr().id,
            now,
        )?;
        if lease_device != membership.marker.device_id {
            return Err(HostRemoteError::ConnectorLeaseDeviceMismatch);
        }
        let remaining_ms = lease.payload.expires_unix_ms.saturating_sub(now);
        let renew_margin_ms = CONNECTOR_LEASE_RENEW_MARGIN.as_millis() as u64;
        let renew_in_ms = remaining_ms.saturating_sub(renew_margin_ms).max(1_000);
        connector_discovery = Some(MdnsConnectorDiscovery::attach(
            outer,
            membership.marker.controller.controller_id,
            membership.marker.site_id,
            true,
        )?);
        save_connector_export(layout, membership, outer, &lease);
        connector_lease = Some(lease);
        presence_refresh_at = Instant::now() + presence_refresh_interval;
        Instant::now() + Duration::from_millis(renew_in_ms)
    } else {
        Instant::now() + Duration::from_secs(24 * 60 * 60)
    };

    let mut tunnels = JoinSet::new();
    let read_allowed = membership
        .marker
        .effective_grant
        .as_ref()
        .map_or(true, |grant| grant.read);
    let write_allowed = membership
        .marker
        .effective_grant
        .as_ref()
        .is_some_and(|grant| grant.write);
    let shell_allowed = membership
        .marker
        .effective_grant
        .as_ref()
        .is_some_and(|grant| grant.shell);
    let tcp_egress_allowed = membership
        .marker
        .effective_grant
        .as_ref()
        .is_some_and(|grant| grant.tcp_egress);
    let tcp_forward_service = tcp_egress_allowed.then(HostTcpForwardService::new);
    loop {
        tokio::select! {
            message = inner.recv(&mut stream) => {
                let message = message?;
                if message.kind == "session_ping" {
                    let reply = InnerMessage::new("session_pong", message.payload)?;
                    inner.send(&mut stream, &reply).await?;
                    continue;
                }
                let (request_id, message) = unwrap_rpc_request(&message)?;
                let operation = async {
                    let reply = match message.kind.as_str() {
                        "read" => {
                            let reply = match (service.as_ref(), read_allowed, ReadRequest::from_message(&message)) {
                                (Some(service), true, Ok(request)) => service.execute(request).await,
                                (_, false, Ok(_)) | (None, true, Ok(_)) => ReadReply::error(
                                    ReadErrorCode::Denied,
                                    "read is not permitted by this device grant",
                                ),
                                (_, _, Err(_)) => ReadReply::error(
                                    ReadErrorCode::InvalidRequest,
                                    "malformed bounded Read request",
                                ),
                            };
                            reply.into_message()?
                        }
                        "fs_query" => {
                            let reply = match (service.as_ref(), read_allowed, FsQueryRequest::from_message(&message)) {
                                (Some(service), true, Ok(request)) => service.execute_fs_query(request).await,
                                (_, false, Ok(_)) | (None, true, Ok(_)) => FsQueryReply::error(
                                    FsQueryErrorCode::Denied,
                                    "filesystem query is not permitted by this device grant",
                                ),
                                (_, _, Err(_)) => FsQueryReply::error(
                                    FsQueryErrorCode::InvalidRequest,
                                    "malformed bounded filesystem query",
                                ),
                            };
                            reply.into_message()?
                        }
                        "fs_mutation" => {
                            let reply = match (service.as_ref(), write_allowed, FsMutationRequest::from_message(&message)) {
                                (Some(service), true, Ok(request)) => service.execute_fs_mutation_rpc(request_id, request, true).await,
                                (_, false, Ok(_)) | (None, true, Ok(_)) => FsMutationReply::error(
                                    FsMutationErrorCode::Denied,
                                    "filesystem mutation is not permitted by this device grant",
                                ),
                                (_, _, Err(_)) => FsMutationReply::error(
                                    FsMutationErrorCode::InvalidRequest,
                                    "malformed bounded filesystem mutation",
                                ),
                            };
                            reply.into_message()?
                        }
                        "file_transfer" => {
                            let reply = match (
                                file_transfer_service.as_ref(),
                                FileTransferRequest::from_message(&message),
                            ) {
                                (Some(service), Ok(request)) => {
                                    service.execute(request, read_allowed, write_allowed).await
                                }
                                (None, Ok(_)) => FileTransferReply::error(
                                    FileTransferErrorCode::Denied,
                                    "file transfer is not permitted by this device grant",
                                ),
                                (_, Err(_)) => FileTransferReply::error(
                                    FileTransferErrorCode::InvalidRequest,
                                    "malformed bounded file transfer request",
                                ),
                            };
                            reply.into_message()?
                        }
                        "tcp_forward" => {
                            let reply = match (
                                tcp_forward_service.as_ref(),
                                tcp_egress_allowed,
                                TcpForwardRequest::from_message(&message),
                            ) {
                                (Some(service), true, Ok(request)) => service.execute(request, true).await,
                                (_, false, Ok(_)) | (None, true, Ok(_)) => TcpForwardReply::error(
                                    TcpForwardErrorCode::Denied,
                                    "TCP egress is not permitted by this device grant",
                                ),
                                (_, _, Err(_)) => TcpForwardReply::error(
                                    TcpForwardErrorCode::InvalidRequest,
                                    "malformed bounded TCP forward request",
                                ),
                            };
                            reply.into_message()?
                        }
                        "shell_task" => {
                            let reply = match (
                                shell_service.as_ref(),
                                shell_allowed,
                                ShellTaskRequest::from_message(&message),
                            ) {
                                (Some(service), true, Ok(request)) => service.execute(request, true).await,
                                (_, false, Ok(_)) | (None, true, Ok(_)) => ShellTaskReply::error(
                                    ShellTaskErrorCode::Denied,
                                    "Shell task is not permitted by this device grant",
                                ),
                                (_, _, Err(_)) => ShellTaskReply::error(
                                    ShellTaskErrorCode::InvalidRequest,
                                    "malformed bounded Shell task request",
                                ),
                            };
                            reply.into_message()?
                        }
                        _ => ReadReply::error(
                            ReadErrorCode::InvalidRequest,
                            "unsupported v3 RPC request",
                        )
                        .into_message()?,
                    };
                    Ok::<_, HostRemoteError>(reply)
                };
                let reply = complete_rpc_with_progress(&mut inner, &mut stream, request_id, operation).await?;
                let reply = wrap_rpc_reply(request_id, reply)?;
                inner.send(&mut stream, &reply).await?;
            }
            accepted = outer.accept_classified(), if connector && tunnels.len() < MAX_CONNECTOR_TUNNELS => {
                let (protocol, inbound) = accepted?;
                if protocol != IrohProtocol::Connector {
                    continue;
                }
                let lease = connector_lease
                    .as_ref()
                    .ok_or(HostRemoteError::MissingConnectorLease)?
                    .clone();
                let outer = outer.clone();
                let controller_endpoint = endpoint.clone();
                let controller = membership.marker.controller;
                let site_id = membership.marker.site_id;
                let device_id = membership.marker.device_id;
                tunnels.spawn(async move {
                    serve_one_connector_tunnel(
                        &outer,
                        inbound,
                        controller_endpoint,
                        controller,
                        site_id,
                        device_id,
                        lease,
                    )
                    .await
                });
            }
            joined = tunnels.join_next(), if !tunnels.is_empty() => {
                if let Some(Err(error)) = joined {
                    return Err(HostRemoteError::ConnectorTask(error.to_string()));
                }
            }
            _ = tokio::time::sleep_until(presence_refresh_at), if connector => {
                let discovery = connector_discovery
                    .as_ref()
                    .ok_or(HostRemoteError::MissingConnectorLease)?;
                let lease = connector_lease
                    .as_ref()
                    .ok_or(HostRemoteError::MissingConnectorLease)?;
                lease.verify_for_candidate(
                    &membership.marker.controller,
                    membership.marker.site_id,
                    outer.addr().id,
                    unix_ms()?,
                )?;
                discovery.refresh_advertisement(outer)?;
                save_connector_export(layout, membership, outer, lease);
                presence_refresh_at = Instant::now() + presence_refresh_interval;
            }
            _ = tokio::time::sleep_until(renew_at), if connector => {
                drop(connector_discovery.take());
                tunnels.abort_all();
                while tunnels.join_next().await.is_some() {}
                return Err(HostRemoteError::ConnectorLeaseRefreshRequired);
            }
        }
    }
}

fn save_connector_export(
    layout: Option<&StateLayout>,
    membership: &HostMembership,
    outer: &IrohOuter,
    lease: &SignedConnectorLease,
) {
    let Some(layout) = layout else {
        return;
    };
    let Ok(file) = NearbyConnectorFile::from_helper(outer.addr(), lease.clone()) else {
        return;
    };
    let _ = NearbyConnectorStore::new(layout.clone()).save_export(
        &file,
        &membership.marker.controller,
        membership.marker.site_id,
    );
}

async fn serve_one_connector_tunnel(
    outer: &IrohOuter,
    mut inbound: clew_transport::IrohStream,
    controller_endpoint: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    device_id: DeviceId,
    lease: SignedConnectorLease,
) -> Result<(), HostRemoteError> {
    let request = read_connector_open(&mut inbound).await?;
    let expected_tag = SiteDiscoveryTag::derive(controller.controller_id, site_id);
    if request.validate()? != expected_tag {
        return Err(HostRemoteError::ConnectorSiteMismatch);
    }
    let now = unix_ms()?;
    let lease_device = lease.verify_for_candidate(&controller, site_id, outer.addr().id, now)?;
    if lease_device != device_id {
        return Err(HostRemoteError::ConnectorLeaseDeviceMismatch);
    }

    let mut outbound = outer.connect_connector(controller_endpoint).await?;
    write_connector_open(&mut outbound, &request).await?;
    write_connector_ready(&mut inbound, &ConnectorReady::new(lease)).await?;
    match forward_opaque_bidirectional(&mut inbound, &mut outbound).await {
        Ok(_) => Ok(()),
        Err(error) if is_normal_tunnel_close(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn is_normal_tunnel_close(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn unix_ms() -> Result<u64, HostRemoteError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HostRemoteError::ClockBeforeUnixEpoch)?
        .as_millis()
        .try_into()
        .map_err(|_| HostRemoteError::ClockOverflow)
}

fn expect_activated(response: BootstrapResponse) -> Result<DeviceRecord, HostRemoteError> {
    match response {
        BootstrapResponse::Activated(record) => Ok(record),
        BootstrapResponse::Error(error) => Err(HostRemoteError::BootstrapRejected {
            code: error.code,
            message: error.message,
        }),
        BootstrapResponse::Claimed(_) | BootstrapResponse::ActivationConfirmed { .. } => {
            Err(HostRemoteError::UnexpectedBootstrapResponse)
        }
    }
}

fn verify_activated(
    membership: &HostMembership,
    activated: &DeviceRecord,
) -> Result<(), HostRemoteError> {
    if activated.device_id != membership.marker.device_id
        || activated.site_id != membership.marker.site_id
        || activated.enrolled_via_invite_id != membership.marker.invite_id
        || activated.capabilities != membership.device.capabilities
    {
        return Err(HostRemoteError::ActivationScopeMismatch);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum HostRemoteError {
    #[error("this membership has no signed Controller endpoint/read policy")]
    MissingNetworkConfig,
    #[error("this membership has neither EXECUTE nor CONNECTOR capability")]
    ExecutionDisabled,
    #[error("Connector lease is missing from an authenticated Connector session")]
    MissingConnectorLease,
    #[error("Connector lease DeviceId does not match this Host membership")]
    ConnectorLeaseDeviceMismatch,
    #[error("Connector tunnel Site tag does not match this Host membership")]
    ConnectorSiteMismatch,
    #[error("Connector lease renewal requires an authenticated Controller reconnect")]
    ConnectorLeaseRefreshRequired,
    #[error("Connector candidate did not complete its verified dial before timeout")]
    ConnectorCandidateTimeout,
    #[error("Connector tunnel task failed: {0}")]
    ConnectorTask(String),
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock value does not fit in milliseconds")]
    ClockOverflow,
    #[error("no direct Controller or verified nearby Connector member path became available")]
    MemberPathUnavailable,
    #[error("no direct Controller or verified nearby Connector bootstrap path became available")]
    BootstrapPathUnavailable,
    #[error("Controller activation response does not match the persisted membership")]
    ActivationScopeMismatch,
    #[error("Controller returned an unexpected bootstrap response")]
    UnexpectedBootstrapResponse,
    #[error("Controller rejected bootstrap ({code:?}): {message}")]
    BootstrapRejected {
        code: BootstrapErrorCode,
        message: String,
    },
    #[error(transparent)]
    Outer(#[from] clew_transport::IrohOuterError),
    #[error(transparent)]
    ConnectorControl(#[from] ConnectorControlError),
    #[error(transparent)]
    ConnectorDiscovery(#[from] ConnectorDiscoveryError),
    #[error(transparent)]
    ConnectorLease(#[from] ConnectorLeaseError),
    #[error(transparent)]
    SealedBootstrap(#[from] SealedBootstrapError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Bootstrap(#[from] clew_transport::BootstrapProtocolError),
    #[error(transparent)]
    Inner(#[from] clew_transport::InnerSessionError),
    #[error(transparent)]
    Rpc(#[from] clew_transport::RpcProtocolError),
    #[error(transparent)]
    FsQuery(#[from] clew_transport::FsQueryProtocolError),
    #[error(transparent)]
    FsMutation(#[from] clew_transport::FsMutationProtocolError),
    #[error(transparent)]
    FileTransfer(#[from] clew_transport::FileTransferError),
    #[error(transparent)]
    TcpForward(#[from] clew_transport::TcpForwardProtocolError),
    #[error(transparent)]
    ShellTask(#[from] clew_transport::ShellTaskProtocolError),
    #[error(transparent)]
    Read(#[from] clew_transport::ReadProtocolError),
    #[error(transparent)]
    Membership(#[from] crate::HostMembershipError),
    #[error(transparent)]
    IdentityStore(#[from] clew_identity::DeviceIdentityStoreError),
    #[error(transparent)]
    Model(#[from] clew_core::ControlModelError),
}

impl HostRemoteError {
    #[must_use]
    pub fn is_retryable_activation(&self) -> bool {
        matches!(
            self,
            Self::BootstrapPathUnavailable
                | Self::Outer(_)
                | Self::ConnectorControl(_)
                | Self::ConnectorDiscovery(_)
                | Self::ConnectorLease(_)
                | Self::SealedBootstrap(_)
                | Self::Bootstrap(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::{DeviceNameOrigin, InviteId, ReadPolicy, RequestId};
    use clew_identity::{
        ControllerIdentity, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
    };
    use clew_transport::{
        BootstrapRequest, BootstrapResponse, noise_static_public, unwrap_rpc_reply,
        wrap_rpc_request,
    };
    use iroh::{EndpointAddr, SecretKey};
    use tempfile::tempdir;

    async fn rpc_roundtrip(
        inner: &mut InnerSession,
        stream: &mut clew_transport::IrohStream,
        message: InnerMessage,
    ) -> InnerMessage {
        let request_id = RequestId::new();
        let message = wrap_rpc_request(request_id, message).unwrap();
        inner.send(stream, &message).await.unwrap();
        let reply = inner.recv(stream).await.unwrap();
        unwrap_rpc_reply(request_id, &reply).unwrap()
    }

    #[tokio::test]
    async fn activation_retry_is_immediately_cancellable() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("cancel-state"));
        let controller = ControllerIdentity::from_secret([101_u8; 32]);
        let controller_public = controller.public_identity();
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let now = unix_ms().unwrap();
        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant::EXECUTE_READ_CONNECTOR,
        );
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Cancel Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
                    not_before_unix_ms: now.saturating_sub(1_000),
                    expires_unix_ms: now + 60_000,
                    deployment_window_ms: 60_000,
                    max_claims: 1,
                },
            )
            .unwrap();
        let bootstrap_secret = [102_u8; 32];
        let site_file = crate::SignedSiteClew::issue_networked_outfit_sealed(
            &controller,
            crate::OutfitProfile::preset(crate::OutfitPreset::ClewOriginal),
            bootstrap,
            crate::HostRoleHint::ExecutePreferred,
            EndpointAddr::new(SecretKey::from_bytes(&[103_u8; 32]).public()),
            ReadPolicy::new(
                vec![temp.path().to_string_lossy().into_owned()],
                4_096,
                2_000,
            )
            .unwrap(),
            noise_static_public(bootstrap_secret).unwrap(),
        )
        .unwrap();
        let pending = DeviceIdentityStore::new(layout.clone())
            .prepare_pending(controller_public, site_id, invite_id)
            .unwrap();
        let state = HostLaunchState::AwaitingEnrollment {
            site_file,
            pending,
            hostname: "CANCEL-TARGET".into(),
            launch_mode: crate::HostLaunchMode::Default,
            source: crate::HostSiteSource::Explicit,
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let wait = wait_for_networked_activation_until_with_timing(
            &layout,
            state,
            shutdown_rx,
            Duration::from_secs(30),
            Duration::from_secs(1),
        );
        tokio::pin!(wait);
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), &mut wait)
            .await
            .expect("activation retry ignored shutdown")
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn helper_launch_mode_is_a_monotonic_authority_downgrade() {
        assert_eq!(
            bootstrap_member_mode(
                crate::HostRoleHint::ExecutePreferred,
                crate::HostLaunchMode::Default,
            ),
            BootstrapMemberMode::ExecutePreferred
        );
        assert_eq!(
            bootstrap_member_mode(
                crate::HostRoleHint::ExecutePreferred,
                crate::HostLaunchMode::ConnectorOnly,
            ),
            BootstrapMemberMode::ConnectorOnly
        );
        assert_eq!(
            bootstrap_member_mode(
                crate::HostRoleHint::ConnectorOnly,
                crate::HostLaunchMode::Default,
            ),
            BootstrapMemberMode::ConnectorOnly
        );
        assert_eq!(
            bootstrap_member_mode(
                crate::HostRoleHint::ConnectorOnly,
                crate::HostLaunchMode::ConnectorOnly,
            ),
            BootstrapMemberMode::ConnectorOnly
        );
    }

    #[tokio::test]
    async fn connector_presence_timer_republishes_export_without_session_reconnect() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("presence-state"));
        let controller = ControllerIdentity::from_secret([107_u8; 32]);
        let controller_public = controller.public_identity();
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let now = unix_ms().unwrap();
        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant::EXECUTE_READ_CONNECTOR,
        );
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Presence Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
                    not_before_unix_ms: now.saturating_sub(1_000),
                    expires_unix_ms: now + 120_000,
                    deployment_window_ms: 120_000,
                    max_claims: 1,
                },
            )
            .unwrap();
        let pending = DeviceIdentityStore::new(layout.clone())
            .prepare_pending(controller_public, site_id, invite_id)
            .unwrap();
        let receipt = registry
            .claim(&bootstrap, pending.public_identity(), now)
            .unwrap();
        let controller_outer = IrohOuter::bind_direct_only().await.unwrap();
        let controller_addr = controller_outer.addr();
        let host_outer = IrohOuter::bind_direct_only().await.unwrap();
        let membership = HostMembershipStore::new(layout.clone())
            .activate_networked(
                crate::ClientFlavor::clew_original_current(),
                None,
                "Presence Lab",
                &pending,
                &receipt,
                "PRESENCE-HELPER",
                controller_addr.clone(),
                ReadPolicy::new(
                    vec![temp.path().to_string_lossy().into_owned()],
                    4_096,
                    2_000,
                )
                .unwrap(),
                None,
            )
            .unwrap();
        registry
            .finalize_host_persist(invite_id, receipt.device_id, receipt.persist_ack_token())
            .unwrap();
        DeviceIdentityStore::new(layout.clone())
            .confirm_controller_activation(controller.controller_id(), site_id, receipt.device_id)
            .unwrap();

        let expected_device = membership.identity.public_identity();
        let expected_device_id = membership.marker.device_id;
        let expected_endpoint_id = host_outer.addr().id;
        let (controller_release_tx, controller_release_rx) = tokio::sync::oneshot::channel();
        let controller_task = tokio::spawn({
            let controller_outer = controller_outer.clone();
            let controller = controller.clone();
            async move {
                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::InnerSession);
                assert_eq!(stream.connection().remote_id(), expected_endpoint_id);
                let mut inner = InnerSession::accept(
                    &mut stream,
                    clew_transport::ControllerSessionIdentity {
                        identity: controller.clone(),
                        noise_static_secret: [108_u8; 32],
                        expected_device,
                        device_id: expected_device_id,
                        site_id,
                    },
                )
                .await
                .unwrap();
                let lease = SignedConnectorLease::issue(
                    &controller,
                    site_id,
                    expected_device_id,
                    expected_endpoint_id,
                    now.saturating_sub(1_000),
                    now + 60_000,
                )
                .unwrap();
                inner
                    .send(&mut stream, &lease.into_message().unwrap())
                    .await
                    .unwrap();
                let _ = controller_release_rx.await;
            }
        });

        let host_task = tokio::spawn({
            let layout = layout.clone();
            let membership = membership.clone();
            let host_outer = host_outer.clone();
            let controller_addr = controller_addr.clone();
            async move {
                serve_networked_membership_with_outer_timing(
                    &membership,
                    &host_outer,
                    controller_addr,
                    &None,
                    &None,
                    &None,
                    Some(&layout),
                    Duration::from_millis(100),
                )
                .await
            }
        });

        let export_path = layout.nearby_connector_export_path(controller.controller_id(), site_id);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !export_path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial Connector export was not created");
        std::fs::remove_file(&export_path).unwrap();
        assert!(!export_path.exists());

        tokio::time::timeout(Duration::from_secs(2), async {
            while !export_path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("presence refresh timer did not recreate the Connector export");
        let refreshed = NearbyConnectorStore::new(layout.clone())
            .load_export(&controller_public, site_id)
            .unwrap()
            .expect("refreshed Connector export did not verify");
        assert_eq!(refreshed.candidate.id, expected_endpoint_id);
        assert_eq!(
            refreshed.lease.payload.connector_device_id,
            expected_device_id
        );

        let _ = controller_release_tx.send(());
        controller_task.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), host_task)
            .await
            .expect("Host session did not notice Controller close after presence test")
            .unwrap()
            .expect_err("single-session helper unexpectedly stayed alive after Controller close");
        host_outer.close().await;
        controller_outer.close().await;
    }

    #[test]
    fn connector_candidate_queue_is_bounded_and_deduplicated() {
        let mut queue = ConnectorCandidateQueue::default();
        let first = EndpointAddr::new(SecretKey::from_bytes(&[1_u8; 32]).public());
        queue.push(first.clone());
        queue.push(first);
        for byte in 2_u8..=40 {
            queue.push(EndpointAddr::new(
                SecretKey::from_bytes(&[byte; 32]).public(),
            ));
        }
        assert_eq!(queue.seen.len(), MAX_CONNECTOR_CANDIDATES_PER_WINDOW);
        assert_eq!(queue.pending.len(), MAX_CONNECTOR_CANDIDATES_PER_WINDOW);
    }

    #[tokio::test]
    async fn stalled_connector_dial_does_not_block_healthy_candidate() {
        let controller = ControllerIdentity::from_secret([104_u8; 32]);
        let controller_public = controller.public_identity();
        let site_id = SiteId::new();
        let now = unix_ms().unwrap();
        let target_outer = IrohOuter::bind_direct_only().await.unwrap();
        let bad_outer = IrohOuter::bind_direct_only().await.unwrap();
        let healthy_outer = IrohOuter::bind_direct_only().await.unwrap();
        let bad_addr = bad_outer.addr();
        let healthy_addr = healthy_outer.addr();
        let healthy_lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            DeviceId::new(),
            healthy_addr.id,
            now.saturating_sub(1_000),
            now + 60_000,
        )
        .unwrap();

        let (bad_open_tx, bad_open_rx) = tokio::sync::oneshot::channel();
        let bad_task = tokio::spawn({
            let bad_outer = bad_outer.clone();
            async move {
                let (protocol, mut stream) = bad_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                let _ = bad_open_tx.send(());
                tokio::time::sleep(Duration::from_secs(30)).await;
                stream
            }
        });
        let (healthy_release_tx, healthy_release_rx) = tokio::sync::oneshot::channel();
        let healthy_task = tokio::spawn({
            let healthy_outer = healthy_outer.clone();
            async move {
                let (protocol, mut stream) = healthy_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                bad_open_rx
                    .await
                    .expect("stalled Helper never received its ConnectorOpen");
                write_connector_ready(&mut stream, &ConnectorReady::new(healthy_lease))
                    .await
                    .unwrap();
                let _ = healthy_release_rx.await;
                stream
            }
        });

        let mut queue = ConnectorCandidateQueue::default();
        queue.push(bad_addr);
        queue.push(healthy_addr.clone());
        let mut tasks = JoinSet::new();
        fill_member_candidate_dials(
            &mut tasks,
            &mut queue,
            &target_outer,
            controller_public,
            site_id,
        );
        assert_eq!(tasks.len(), 2);

        let winner = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tasks.join_next().await {
                    Some(Ok(Ok(stream))) => break stream,
                    Some(Ok(Err(_))) => continue,
                    Some(Err(error)) => panic!("candidate dial task failed: {error}"),
                    None => panic!("all candidate dials ended without a healthy Helper"),
                }
            }
        })
        .await
        .expect("healthy Helper was blocked behind the stalled candidate");
        assert_eq!(winner.connection().remote_id(), healthy_addr.id);

        tasks.abort_all();
        drop(winner);
        let _ = healthy_release_tx.send(());
        healthy_task.await.unwrap();
        bad_task.abort();
        let _ = bad_task.await;
        target_outer.close().await;
        healthy_outer.close().await;
        bad_outer.close().await;
    }

    #[tokio::test]
    #[ignore = "requires working multicast mDNS on the test network"]
    async fn real_mdns_healthy_helper_wins_while_first_helper_stalls() {
        let controller = ControllerIdentity::from_secret([105_u8; 32]);
        let controller_public = controller.public_identity();
        let site_id = SiteId::new();
        let now = unix_ms().unwrap();
        let target_outer = IrohOuter::bind_direct_only().await.unwrap();
        let bad_outer = IrohOuter::bind_direct_only().await.unwrap();
        let healthy_outer = IrohOuter::bind_direct_only().await.unwrap();
        let bad_addr = bad_outer.addr();
        let healthy_addr = healthy_outer.addr();
        let _bad_advertisement = MdnsConnectorDiscovery::attach(
            &bad_outer,
            controller_public.controller_id,
            site_id,
            true,
        )
        .unwrap();
        let healthy_lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            DeviceId::new(),
            healthy_addr.id,
            now.saturating_sub(1_000),
            now + 60_000,
        )
        .unwrap();

        let (bad_open_tx, bad_open_rx) = tokio::sync::oneshot::channel();
        let bad_task = tokio::spawn({
            let bad_outer = bad_outer.clone();
            async move {
                let (protocol, mut stream) = bad_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                let _ = bad_open_tx.send(());
                tokio::time::sleep(Duration::from_secs(30)).await;
                stream
            }
        });
        let (healthy_release_tx, healthy_release_rx) = tokio::sync::oneshot::channel();
        let healthy_task = tokio::spawn({
            let healthy_outer = healthy_outer.clone();
            async move {
                bad_open_rx
                    .await
                    .expect("stalled Helper never received its ConnectorOpen");
                let _healthy_advertisement = MdnsConnectorDiscovery::attach(
                    &healthy_outer,
                    controller_public.controller_id,
                    site_id,
                    true,
                )
                .unwrap();
                let (protocol, mut stream) = healthy_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                write_connector_ready(&mut stream, &ConnectorReady::new(healthy_lease))
                    .await
                    .unwrap();
                let _ = healthy_release_rx.await;
                stream
            }
        });

        let bogus_controller = EndpointAddr::new(SecretKey::from_bytes(&[106_u8; 32]).public());
        let (winner, path) = tokio::time::timeout(
            Duration::from_secs(8),
            connect_member_stream(
                None,
                &target_outer,
                bogus_controller,
                controller_public,
                site_id,
            ),
        )
        .await
        .expect("healthy mDNS Helper was blocked behind the stalled first candidate")
        .unwrap();
        assert_eq!(path, MemberOuterPath::Connector);
        assert_eq!(winner.connection().remote_id(), healthy_addr.id);
        assert_ne!(winner.connection().remote_id(), bad_addr.id);

        drop(winner);
        let _ = healthy_release_tx.send(());
        healthy_task.await.unwrap();
        bad_task.abort();
        let _ = bad_task.await;
        target_outer.close().await;
        healthy_outer.close().await;
        bad_outer.close().await;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailoverDiscovery {
        Mdns,
        NearbyFile,
    }

    #[tokio::test]
    async fn active_session_fails_over_between_nearby_file_helpers() {
        run_active_helper_failover(FailoverDiscovery::NearbyFile).await;
    }

    #[tokio::test]
    #[ignore = "requires working multicast mDNS on the test network"]
    async fn active_session_fails_over_between_real_mdns_helpers() {
        run_active_helper_failover(FailoverDiscovery::Mdns).await;
    }

    async fn run_active_helper_failover(discovery: FailoverDiscovery) {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("failover-state"));
        let share = temp.path().join("share");
        std::fs::create_dir_all(&share).unwrap();
        let proof_a = share.join("a.txt");
        let proof_b = share.join("b.txt");
        const PROOF_A: &[u8] = b"CLEW-V15D-HELPER-A";
        const PROOF_B: &[u8] = b"CLEW-V15D-HELPER-B";
        std::fs::write(&proof_a, PROOF_A).unwrap();
        std::fs::write(&proof_b, PROOF_B).unwrap();

        let controller = ControllerIdentity::from_secret([121_u8; 32]);
        let controller_public = controller.public_identity();
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let now = unix_ms().unwrap();
        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant::EXECUTE_READ_CONNECTOR,
        );
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Failover Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
                    not_before_unix_ms: now.saturating_sub(1_000),
                    expires_unix_ms: now + 120_000,
                    deployment_window_ms: 120_000,
                    max_claims: 1,
                },
            )
            .unwrap();
        let pending = DeviceIdentityStore::new(layout.clone())
            .prepare_pending(controller_public, site_id, invite_id)
            .unwrap();
        let receipt = registry
            .claim(&bootstrap, pending.public_identity(), now)
            .unwrap();

        let controller_outer = IrohOuter::bind_direct_only().await.unwrap();
        let controller_addr = controller_outer.addr();
        let helper_a_outer = IrohOuter::bind_direct_only().await.unwrap();
        let helper_b_outer = IrohOuter::bind_direct_only().await.unwrap();
        let helper_a_addr = helper_a_outer.addr();
        let helper_b_addr = helper_b_outer.addr();
        let helper_a_device = DeviceId::new();
        let helper_b_device = DeviceId::new();
        let lease_a = SignedConnectorLease::issue(
            &controller,
            site_id,
            helper_a_device,
            helper_a_addr.id,
            now.saturating_sub(1_000),
            now + 120_000,
        )
        .unwrap();
        let lease_b = SignedConnectorLease::issue(
            &controller,
            site_id,
            helper_b_device,
            helper_b_addr.id,
            now.saturating_sub(1_000),
            now + 120_000,
        )
        .unwrap();

        let bogus_controller = EndpointAddr::new(SecretKey::from_bytes(&[122_u8; 32]).public());
        let read_policy =
            ReadPolicy::new(vec![share.to_string_lossy().into_owned()], 4_096, 2_000).unwrap();
        let membership = HostMembershipStore::new(layout.clone())
            .activate_networked(
                crate::ClientFlavor::clew_original_current(),
                None,
                "Failover Lab",
                &pending,
                &receipt,
                "FAILOVER-TARGET",
                bogus_controller,
                read_policy,
                None,
            )
            .unwrap();
        registry
            .finalize_host_persist(invite_id, receipt.device_id, receipt.persist_ack_token())
            .unwrap();
        DeviceIdentityStore::new(layout.clone())
            .confirm_controller_activation(controller.controller_id(), site_id, receipt.device_id)
            .unwrap();
        let expected_device = membership.identity.public_identity();
        let expected_device_id = membership.marker.device_id;

        let nearby_store = NearbyConnectorStore::new(layout.clone());
        let mut mdns_a = None;
        match discovery {
            FailoverDiscovery::Mdns => {
                mdns_a = Some(
                    MdnsConnectorDiscovery::attach(
                        &helper_a_outer,
                        controller_public.controller_id,
                        site_id,
                        true,
                    )
                    .unwrap(),
                );
            }
            FailoverDiscovery::NearbyFile => {
                let file = NearbyConnectorFile::from_helper(helper_a_addr.clone(), lease_a.clone())
                    .unwrap();
                nearby_store
                    .import_file(&file, &controller_public, site_id)
                    .unwrap();
            }
        }

        let helper_a_task = tokio::spawn({
            let helper_a_outer = helper_a_outer.clone();
            let controller_addr = controller_addr.clone();
            async move {
                let (protocol, inbound) = helper_a_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                serve_one_connector_tunnel(
                    &helper_a_outer,
                    inbound,
                    controller_addr,
                    controller_public,
                    site_id,
                    helper_a_device,
                    lease_a,
                )
                .await
            }
        });
        let helper_b_lease = lease_b.clone();
        let helper_b_task = tokio::spawn({
            let helper_b_outer = helper_b_outer.clone();
            let controller_addr = controller_addr.clone();
            async move {
                let (protocol, inbound) = helper_b_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                serve_one_connector_tunnel(
                    &helper_b_outer,
                    inbound,
                    controller_addr,
                    controller_public,
                    site_id,
                    helper_b_device,
                    helper_b_lease,
                )
                .await
            }
        });

        let (first_done_tx, first_done_rx) = tokio::sync::oneshot::channel();
        let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
        let (second_done_tx, second_done_rx) = tokio::sync::oneshot::channel();
        let controller_task = tokio::spawn({
            let controller_outer = controller_outer.clone();
            let controller = controller.clone();
            let proof_a = proof_a.to_string_lossy().into_owned();
            let proof_b = proof_b.to_string_lossy().into_owned();
            async move {
                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                assert_eq!(stream.connection().remote_id(), helper_a_addr.id);
                let open = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(open.purpose, ConnectorTunnelPurpose::InnerSession);
                let mut inner = InnerSession::accept(
                    &mut stream,
                    clew_transport::ControllerSessionIdentity {
                        identity: controller.clone(),
                        noise_static_secret: [123_u8; 32],
                        expected_device,
                        device_id: expected_device_id,
                        site_id,
                    },
                )
                .await
                .unwrap();
                let first_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ReadRequest::new(proof_a, 0, 4_096)
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                assert_eq!(
                    ReadReply::from_message(&first_reply).unwrap(),
                    ReadReply::Data(PROOF_A.to_vec())
                );
                let _ = first_done_tx.send(());
                let _ = first_release_rx.await;
                drop(inner);
                drop(stream);

                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                assert_eq!(stream.connection().remote_id(), helper_b_addr.id);
                let open = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(open.purpose, ConnectorTunnelPurpose::InnerSession);
                let mut inner = InnerSession::accept(
                    &mut stream,
                    clew_transport::ControllerSessionIdentity {
                        identity: controller,
                        noise_static_secret: [123_u8; 32],
                        expected_device,
                        device_id: expected_device_id,
                        site_id,
                    },
                )
                .await
                .unwrap();
                let second_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ReadRequest::new(proof_b, 0, 4_096)
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                assert_eq!(
                    ReadReply::from_message(&second_reply).unwrap(),
                    ReadReply::Data(PROOF_B.to_vec())
                );
                let _ = second_done_tx.send(());
            }
        });

        let (target_shutdown_tx, target_shutdown_rx) = tokio::sync::oneshot::channel();
        let target_task = tokio::spawn({
            let layout = layout.clone();
            let membership = membership.clone();
            async move {
                serve_networked_membership_until_with_layout(&layout, &membership, async move {
                    let _ = target_shutdown_rx.await;
                })
                .await
            }
        });

        tokio::time::timeout(Duration::from_secs(12), first_done_rx)
            .await
            .expect("Helper-A never carried the first active Read")
            .unwrap();
        drop(mdns_a.take());
        helper_a_outer.close().await;

        let mut mdns_b = None;
        match discovery {
            FailoverDiscovery::Mdns => {
                mdns_b = Some(
                    MdnsConnectorDiscovery::attach(
                        &helper_b_outer,
                        controller_public.controller_id,
                        site_id,
                        true,
                    )
                    .unwrap(),
                );
            }
            FailoverDiscovery::NearbyFile => {
                let file = NearbyConnectorFile::from_helper(helper_b_addr.clone(), lease_b.clone())
                    .unwrap();
                nearby_store
                    .import_file(&file, &controller_public, site_id)
                    .unwrap();
            }
        }
        let _ = first_release_tx.send(());

        tokio::time::timeout(Duration::from_secs(15), second_done_rx)
            .await
            .expect("Target did not fail over from Helper-A to Helper-B")
            .unwrap();
        let _ = target_shutdown_tx.send(());
        controller_task.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), target_task)
            .await
            .expect("Target failover runtime did not stop after shutdown")
            .unwrap()
            .unwrap();

        drop(mdns_b.take());
        helper_b_outer.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), helper_b_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), helper_a_task).await;
        controller_outer.close().await;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum NoPublicDiscovery {
        MdnsDelayed,
        NearbyFile,
        NearbyFileConnectorOnly,
    }

    #[tokio::test]
    #[ignore = "requires working multicast mDNS on the test network"]
    async fn sealed_bootstrap_and_active_read_use_real_mdns_connector_when_direct_is_unreachable() {
        run_no_public_connector_flow(NoPublicDiscovery::MdnsDelayed).await;
    }

    #[tokio::test]
    async fn sealed_bootstrap_and_active_read_use_nearby_file_when_mdns_is_unavailable() {
        run_no_public_connector_flow(NoPublicDiscovery::NearbyFile).await;
    }

    #[tokio::test]
    async fn connector_only_launch_intent_downgrades_the_same_signed_site_kit() {
        run_no_public_connector_flow(NoPublicDiscovery::NearbyFileConnectorOnly).await;
    }

    async fn run_no_public_connector_flow(discovery_mode: NoPublicDiscovery) {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path().join("target-state"));
        let controller = ControllerIdentity::from_secret([111_u8; 32]);
        let controller_public = controller.public_identity();
        let controller_bootstrap_secret = [112_u8; 32];
        let controller_bootstrap_public = noise_static_public(controller_bootstrap_secret).unwrap();
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let helper_device_id = DeviceId::new();
        let now = unix_ms().unwrap();

        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant::EXECUTE_READ_WRITE_SHELL_TCP_CONNECTOR,
        );
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Connector Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_WRITE_SHELL_TCP_CONNECTOR,
                    not_before_unix_ms: now.saturating_sub(1_000),
                    expires_unix_ms: now + 60_000,
                    deployment_window_ms: 60_000,
                    max_claims: 2,
                },
            )
            .unwrap();

        let controller_outer = IrohOuter::bind_direct_only().await.unwrap();
        let controller_addr = controller_outer.addr();
        let helper_outer = IrohOuter::bind_direct_only().await.unwrap();
        let helper_addr = helper_outer.addr();
        let helper_lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            helper_device_id,
            helper_addr.id,
            now.saturating_sub(1_000),
            now + 60_000,
        )
        .unwrap();
        if matches!(
            discovery_mode,
            NoPublicDiscovery::NearbyFile | NoPublicDiscovery::NearbyFileConnectorOnly
        ) {
            let file = NearbyConnectorFile::from_helper(helper_addr.clone(), helper_lease.clone())
                .unwrap();
            NearbyConnectorStore::new(layout.clone())
                .import_file(&file, &controller_public, site_id)
                .unwrap();
        }

        let controller_task = tokio::spawn({
            let controller_outer = controller_outer.clone();
            let controller = controller.clone();
            async move {
                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let open = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(open.purpose, ConnectorTunnelPurpose::Bootstrap);
                assert_eq!(
                    open.validate().unwrap(),
                    SiteDiscoveryTag::derive(controller.controller_id(), site_id)
                );
                let mut sealed = SealedBootstrapSession::accept(
                    &mut stream,
                    SealedBootstrapContext {
                        controller_id: controller.controller_id(),
                        site_id,
                    },
                    controller_bootstrap_secret,
                )
                .await
                .unwrap();
                let first: BootstrapRequest = sealed.recv(&mut stream).await.unwrap();
                let (pass, device_identity, hostname, mode) = match first {
                    BootstrapRequest::Claim {
                        bootstrap,
                        device_identity,
                        hostname,
                        mode,
                    } => (bootstrap, device_identity, hostname, mode),
                    _ => panic!("expected Claim"),
                };
                let ceiling = match mode {
                    BootstrapMemberMode::ExecutePreferred => {
                        PermissionGrant::EXECUTE_READ_WRITE_SHELL_TCP_CONNECTOR
                    }
                    BootstrapMemberMode::ConnectorOnly => PermissionGrant::CONNECTOR_ONLY,
                };
                let receipt = registry
                    .claim_with_ceiling(&pass, device_identity, now, ceiling)
                    .unwrap();
                if mode == BootstrapMemberMode::ConnectorOnly {
                    assert!(!receipt.effective_grant.member.execute);
                    assert!(receipt.effective_grant.member.connector);
                    assert!(!receipt.effective_grant.read);
                    assert!(!receipt.effective_grant.write);
                    assert!(!receipt.effective_grant.shell);
                    assert!(!receipt.effective_grant.tcp_egress);
                }
                sealed
                    .send(&mut stream, &BootstrapResponse::Claimed(receipt.clone()))
                    .await
                    .unwrap();
                let persisted: BootstrapRequest = sealed.recv(&mut stream).await.unwrap();
                let persist_token = match persisted {
                    BootstrapRequest::Persisted {
                        invite_id: actual_invite,
                        device_id,
                        persist_ack_token,
                        hostname: persisted_hostname,
                    } => {
                        assert_eq!(actual_invite, receipt.invite_id);
                        assert_eq!(device_id, receipt.device_id);
                        assert_eq!(persisted_hostname, hostname);
                        persist_ack_token
                    }
                    _ => panic!("expected Persisted"),
                };
                let enrollment = registry
                    .finalize_host_persist(receipt.invite_id, receipt.device_id, &persist_token)
                    .unwrap();
                let normalized = crate::normalize_hostname(&hostname);
                let record = DeviceRecord {
                    device_id: receipt.device_id,
                    site_id,
                    display_name: normalized.clone(),
                    hostname_observed: normalized.clone(),
                    capabilities: enrollment.effective_grant.member,
                    enrolled_via_invite_id: receipt.invite_id,
                    name_origin: DeviceNameOrigin::Automatic {
                        base_hostname: normalized,
                        tagged: false,
                        tag_generation: 0,
                    },
                };
                sealed
                    .send(&mut stream, &BootstrapResponse::Activated(record.clone()))
                    .await
                    .unwrap();
                let ack: BootstrapRequest = sealed.recv(&mut stream).await.unwrap();
                assert!(matches!(
                    ack,
                    BootstrapRequest::ActivatedAck { invite_id: actual_invite, device_id }
                        if actual_invite == receipt.invite_id && device_id == receipt.device_id
                ));
                sealed
                    .send(
                        &mut stream,
                        &BootstrapResponse::ActivationConfirmed {
                            invite_id: receipt.invite_id,
                            device_id: receipt.device_id,
                        },
                    )
                    .await
                    .unwrap();
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    sealed.recv::<_, BootstrapRequest>(&mut stream),
                )
                .await;
                record
            }
        });

        let helper_task = tokio::spawn({
            let helper_outer = helper_outer.clone();
            let controller_addr = controller_addr.clone();
            let use_mdns = discovery_mode == NoPublicDiscovery::MdnsDelayed;
            async move {
                let _advertisement = if use_mdns {
                    tokio::time::sleep(Duration::from_millis(8_500)).await;
                    Some(
                        MdnsConnectorDiscovery::attach(
                            &helper_outer,
                            controller_public.controller_id,
                            site_id,
                            true,
                        )
                        .unwrap(),
                    )
                } else {
                    None
                };
                let (protocol, inbound) = helper_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                serve_one_connector_tunnel(
                    &helper_outer,
                    inbound,
                    controller_addr,
                    controller_public,
                    site_id,
                    helper_device_id,
                    helper_lease,
                )
                .await
                .unwrap();
            }
        });

        let bogus_controller = EndpointAddr::new(SecretKey::from_bytes(&[113_u8; 32]).public());
        let read_policy = ReadPolicy::new(
            vec![temp.path().join("share").to_string_lossy().into_owned()],
            4_096,
            2_000,
        )
        .unwrap();
        let profile = crate::OutfitProfile::preset(crate::OutfitPreset::ClewOriginal);
        let site_file = crate::SignedSiteClew::issue_networked_outfit_sealed(
            &controller,
            profile,
            bootstrap,
            crate::HostRoleHint::ExecutePreferred,
            bogus_controller,
            read_policy,
            controller_bootstrap_public,
        )
        .unwrap();
        let pending = DeviceIdentityStore::new(layout.clone())
            .prepare_pending(controller_public, site_id, invite_id)
            .unwrap();
        let initial = HostLaunchState::AwaitingEnrollment {
            site_file,
            pending,
            hostname: "NO-PUBLIC-TARGET".into(),
            launch_mode: if discovery_mode == NoPublicDiscovery::NearbyFileConnectorOnly {
                crate::HostLaunchMode::ConnectorOnly
            } else {
                crate::HostLaunchMode::Default
            },
            source: crate::HostSiteSource::Explicit,
        };
        let (_activation_shutdown_tx, activation_shutdown_rx) = tokio::sync::watch::channel(false);
        let activation_started = Instant::now();
        let (activation_timeout, path_window) = match discovery_mode {
            NoPublicDiscovery::MdnsDelayed => (Duration::from_secs(35), Duration::from_secs(8)),
            NoPublicDiscovery::NearbyFile | NoPublicDiscovery::NearbyFileConnectorOnly => {
                (Duration::from_secs(15), Duration::from_secs(10))
            }
        };
        let activated = tokio::time::timeout(
            activation_timeout,
            wait_for_networked_activation_until_with_timing(
                &layout,
                initial,
                activation_shutdown_rx,
                path_window,
                Duration::from_millis(25),
            ),
        )
        .await
        .expect("Connector bootstrap activation timed out")
        .unwrap()
        .expect("activation retry loop stopped unexpectedly");
        if discovery_mode == NoPublicDiscovery::MdnsDelayed {
            assert!(
                activation_started.elapsed() >= Duration::from_secs(8),
                "Target unexpectedly activated before the delayed Helper was online"
            );
        }
        let activated_is_connector_only = activated.is_connector_only();
        let HostLaunchState::Active { membership, .. } = activated else {
            panic!("Target did not become active");
        };
        if discovery_mode == NoPublicDiscovery::NearbyFileConnectorOnly {
            assert!(!membership.device.capabilities.execute);
            assert!(membership.device.capabilities.connector);
            assert!(activated_is_connector_only);
        } else {
            assert!(membership.device.capabilities.execute);
            assert!(membership.device.capabilities.connector);
        }
        if discovery_mode != NoPublicDiscovery::NearbyFileConnectorOnly {
            assert!(
                membership
                    .marker
                    .effective_grant
                    .as_ref()
                    .is_some_and(|grant| grant.read && grant.write && grant.shell)
            );
        }
        assert_eq!(
            membership.marker.controller_bootstrap_noise_public_key,
            Some(controller_bootstrap_public)
        );
        let controller_record = controller_task.await.unwrap();
        assert_eq!(controller_record.device_id, membership.marker.device_id);
        tokio::time::timeout(Duration::from_secs(5), helper_task)
            .await
            .expect("Helper tunnel did not close after activation")
            .unwrap();

        if discovery_mode == NoPublicDiscovery::NearbyFileConnectorOnly {
            helper_outer.close().await;
            controller_outer.close().await;
            return;
        }

        let share = temp.path().join("share");
        std::fs::create_dir_all(&share).unwrap();
        let proof_path = share.join("proof.txt");
        const READ_PROOF: &[u8] = b"CLEW-V15C-NO-PUBLIC-CONNECTOR-READ";
        std::fs::write(&proof_path, READ_PROOF).unwrap();

        let read_now = unix_ms().unwrap();
        let helper_read_lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            helper_device_id,
            helper_addr.id,
            read_now.saturating_sub(1_000),
            read_now + 60_000,
        )
        .unwrap();
        let _read_advertisement = if discovery_mode == NoPublicDiscovery::MdnsDelayed {
            Some(
                MdnsConnectorDiscovery::attach(
                    &helper_outer,
                    controller_public.controller_id,
                    site_id,
                    true,
                )
                .unwrap(),
            )
        } else {
            let file =
                NearbyConnectorFile::from_helper(helper_addr.clone(), helper_read_lease.clone())
                    .unwrap();
            NearbyConnectorStore::new(layout.clone())
                .import_file(&file, &controller_public, site_id)
                .unwrap();
            None
        };

        let controller_read_task = tokio::spawn({
            let controller_outer = controller_outer.clone();
            let controller = controller.clone();
            let expected_device = membership.identity.public_identity();
            let device_id = membership.marker.device_id;
            let proof_path = proof_path.to_string_lossy().into_owned();
            let share_path = share.to_string_lossy().into_owned();
            let mutation_path = share.join("mutation.txt").to_string_lossy().into_owned();
            async move {
                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let open = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(open.purpose, ConnectorTunnelPurpose::InnerSession);
                assert_eq!(
                    open.validate().unwrap(),
                    SiteDiscoveryTag::derive(controller.controller_id(), site_id)
                );
                let mut inner = InnerSession::accept(
                    &mut stream,
                    clew_transport::ControllerSessionIdentity {
                        identity: controller,
                        noise_static_secret: [114_u8; 32],
                        expected_device,
                        device_id,
                        site_id,
                    },
                )
                .await
                .unwrap();
                let reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ReadRequest::new(proof_path.clone(), 0, 4_096)
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                assert_eq!(
                    ReadReply::from_message(&reply).unwrap(),
                    ReadReply::Data(READ_PROOF.to_vec())
                );

                let info_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    FsQueryRequest::path_info(proof_path.clone())
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                let FsQueryReply::PathInfo(info) = FsQueryReply::from_message(&info_reply).unwrap()
                else {
                    panic!("expected PathInfo reply over Connector InnerSession");
                };
                assert_eq!(info.kind, clew_transport::FsPathKind::File);
                assert_eq!(info.size, READ_PROOF.len() as u64);
                assert!(info.path.ends_with("proof.txt"));

                let glob_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    FsQueryRequest::glob(share_path.clone(), "*.txt", 0, 8, 4_096)
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                let FsQueryReply::Glob(page) = FsQueryReply::from_message(&glob_reply).unwrap()
                else {
                    panic!("expected Glob reply over Connector InnerSession");
                };
                assert_eq!(page.entries.len(), 1);
                assert!(page.entries[0].path.ends_with("proof.txt"));
                assert!(!page.truncated);

                let grep_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    FsQueryRequest::grep(
                        share_path.clone(),
                        "CLEW-V15C",
                        Some("*.txt".into()),
                        0,
                        8,
                        4_096,
                        4_096,
                    )
                    .unwrap()
                    .into_message()
                    .unwrap(),
                )
                .await;
                let FsQueryReply::Grep(page) = FsQueryReply::from_message(&grep_reply).unwrap()
                else {
                    panic!("expected Grep reply over Connector InnerSession");
                };
                assert_eq!(page.matches.len(), 1);
                assert!(page.matches[0].path.ends_with("proof.txt"));
                assert_eq!(page.matches[0].line_number, 1);
                assert_eq!(page.matches[0].line, "CLEW-V15C-NO-PUBLIC-CONNECTOR-READ");
                assert!(!page.truncated);

                let write_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    FsMutationRequest::write(
                        mutation_path.clone(),
                        "alpha OLD omega\n",
                        clew_transport::FsWritePrecondition::CreateOnly,
                    )
                    .unwrap()
                    .into_message()
                    .unwrap(),
                )
                .await;
                let FsMutationReply::Result(created) =
                    FsMutationReply::from_message(&write_reply).unwrap()
                else {
                    panic!("expected Write reply over Connector InnerSession");
                };
                assert!(created.created);
                assert_eq!(created.size, 16);

                let edit_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    FsMutationRequest::edit(mutation_path.clone(), created.sha256, "OLD", "NEW")
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                let FsMutationReply::Result(edited) =
                    FsMutationReply::from_message(&edit_reply).unwrap()
                else {
                    panic!("expected Edit reply over Connector InnerSession");
                };
                assert!(!edited.created);
                assert_eq!(edited.size, 16);

                let mutated_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ReadRequest::new(mutation_path, 0, 4_096)
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await;
                assert_eq!(
                    ReadReply::from_message(&mutated_reply).unwrap(),
                    ReadReply::Data(b"alpha NEW omega\n".to_vec())
                );

                let shell_command = if cfg!(windows) {
                    "echo CLEW-SHELL-CONNECTOR"
                } else {
                    "printf CLEW-SHELL-CONNECTOR"
                };
                let started_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ShellTaskRequest::start(
                        shell_command,
                        share_path,
                        std::collections::BTreeMap::new(),
                        5_000,
                    )
                    .unwrap()
                    .into_message()
                    .unwrap(),
                )
                .await;
                let ShellTaskReply::Started { task_id } =
                    ShellTaskReply::from_message(&started_reply).unwrap()
                else {
                    panic!("expected Shell start reply over Connector InnerSession");
                };
                let status = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let status_reply = rpc_roundtrip(
                            &mut inner,
                            &mut stream,
                            ShellTaskRequest::Status { task_id }.into_message().unwrap(),
                        )
                        .await;
                        let ShellTaskReply::Status(status) =
                            ShellTaskReply::from_message(&status_reply).unwrap()
                        else {
                            panic!("expected Shell status reply over Connector InnerSession");
                        };
                        if status.phase.terminal() {
                            break status;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("Shell task did not finish over Connector InnerSession");
                assert_eq!(status.phase, clew_transport::ShellTaskPhase::Exited);
                assert_eq!(status.exit_code, Some(0));

                let output_reply = rpc_roundtrip(
                    &mut inner,
                    &mut stream,
                    ShellTaskRequest::Attach {
                        task_id,
                        stdout_offset: 0,
                        stderr_offset: 0,
                        max_bytes_per_stream: 4_096,
                    }
                    .into_message()
                    .unwrap(),
                )
                .await;
                let ShellTaskReply::Output(output) =
                    ShellTaskReply::from_message(&output_reply).unwrap()
                else {
                    panic!("expected Shell output reply over Connector InnerSession");
                };
                assert!(
                    String::from_utf8_lossy(&output.stdout.decode().unwrap())
                        .contains("CLEW-SHELL-CONNECTOR")
                );
            }
        });

        let helper_read_task = tokio::spawn({
            let helper_outer = helper_outer.clone();
            let controller_addr = controller_addr.clone();
            async move {
                let (protocol, inbound) = helper_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                serve_one_connector_tunnel(
                    &helper_outer,
                    inbound,
                    controller_addr,
                    controller_public,
                    site_id,
                    helper_device_id,
                    helper_read_lease,
                )
                .await
                .unwrap();
            }
        });
        let target_membership = membership.clone();
        let target_layout = layout.clone();
        let target_read_task = tokio::spawn(async move {
            serve_networked_membership_once_with_layout(&target_layout, &target_membership).await
        });

        tokio::time::timeout(Duration::from_secs(12), controller_read_task)
            .await
            .expect("Controller did not receive the Connector Read result")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), helper_read_task)
            .await
            .expect("Helper Read tunnel did not close")
            .unwrap();
        target_read_task.abort();
        let _ = target_read_task.await;

        helper_outer.close().await;
        controller_outer.close().await;
    }
}
