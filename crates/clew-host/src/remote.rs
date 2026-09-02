use std::{future::Future, time::Duration};

use clew_core::{DeviceId, DeviceRecord, SiteId, StateLayout};
use clew_identity::{ControllerPublicIdentity, DeviceIdentityStore};
use clew_transport::{
    BootstrapErrorCode, BootstrapMemberMode, BootstrapRequest, BootstrapResponse,
    ConnectorControlError, ConnectorDiscoveryError, ConnectorDiscoveryEvent, ConnectorLeaseError,
    ConnectorOpenRequest, ConnectorReady, ConnectorTunnelPurpose, DeviceSessionIdentity,
    InnerSession, IrohOuter, IrohProtocol, MdnsConnectorDiscovery, NearbyConnectorFile,
    ReadErrorCode, ReadReply, ReadRequest, SealedBootstrapContext, SealedBootstrapError,
    SealedBootstrapSession, SignedConnectorLease, SiteDiscoveryTag, forward_opaque_bidirectional,
    read_bootstrap, read_connector_open, read_connector_ready, write_bootstrap,
    write_connector_open, write_connector_ready,
};
use iroh::EndpointAddr;
use thiserror::Error;
use tokio::{sync::watch, task::JoinSet, time::Instant};

use crate::{
    HostLaunchState, HostMembership, HostMembershipStore, HostReadService, NearbyConnectorStore,
};

const MAX_CONNECTOR_TUNNELS: usize = 64;
const CONNECTOR_LEASE_RENEW_MARGIN: Duration = Duration::from_secs(30);
const INITIAL_BOOTSTRAP_PATH_WINDOW: Duration = Duration::from_secs(20);
const ACTIVE_MEMBER_PATH_WINDOW: Duration = Duration::from_secs(20);

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
                    mode: match site_file.payload.role_hint {
                        crate::HostRoleHint::ExecutePreferred => {
                            BootstrapMemberMode::ExecutePreferred
                        }
                        crate::HostRoleHint::ConnectorOnly => BootstrapMemberMode::ConnectorOnly,
                    },
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
    let (endpoint, service) = member_remote_config(membership)?;
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
    let (endpoint, service) = member_remote_config(membership)?;
    let outer = IrohOuter::bind().await?;
    serve_networked_membership_with_outer(membership, &outer, endpoint, &service, None).await
}

pub async fn serve_networked_membership_once_with_layout(
    layout: &StateLayout,
    membership: &HostMembership,
) -> Result<(), HostRemoteError> {
    let (endpoint, service) = member_remote_config(membership)?;
    let outer = IrohOuter::bind().await?;
    serve_networked_membership_with_outer(membership, &outer, endpoint, &service, Some(layout))
        .await
}

fn member_remote_config(
    membership: &HostMembership,
) -> Result<(EndpointAddr, Option<HostReadService>), HostRemoteError> {
    if !membership.device.capabilities.execute && !membership.device.capabilities.connector {
        return Err(HostRemoteError::ExecutionDisabled);
    }
    let endpoint = membership
        .marker
        .controller_endpoint
        .clone()
        .ok_or(HostRemoteError::MissingNetworkConfig)?;
    let service = if membership.device.capabilities.execute {
        let policy = membership
            .marker
            .read_policy
            .clone()
            .ok_or(HostRemoteError::MissingNetworkConfig)?;
        Some(HostReadService::new(policy)?)
    } else {
        None
    };
    Ok((endpoint, service))
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

async fn connect_bootstrap_channel(
    layout: &StateLayout,
    outer: &IrohOuter,
    controller_endpoint: EndpointAddr,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    controller_bootstrap_noise_public_key: Option<[u8; 32]>,
    path_window: Duration,
) -> Result<HostBootstrapChannel, HostRemoteError> {
    let direct = outer.connect_bootstrap(controller_endpoint);
    tokio::pin!(direct);

    let Some(bootstrap_key) = controller_bootstrap_noise_public_key else {
        return tokio::time::timeout(path_window, &mut direct)
            .await
            .map_err(|_| HostRemoteError::BootstrapPathUnavailable)?
            .map(HostBootstrapChannel::Direct)
            .map_err(HostRemoteError::from);
    };

    let discovery =
        MdnsConnectorDiscovery::attach(outer, controller.controller_id, site_id, false)?;
    let mut events = discovery.subscribe().await;
    let fallback_candidate = NearbyConnectorStore::new(layout.clone())
        .load_import(&controller, site_id)
        .ok()
        .flatten()
        .map(|file| file.connector_candidate().addr);
    let fallback = async {
        match fallback_candidate {
            Some(candidate) => {
                connect_bootstrap_via_candidate(
                    outer,
                    candidate,
                    controller,
                    site_id,
                    bootstrap_key,
                )
                .await
            }
            None => std::future::pending::<Result<HostBootstrapChannel, HostRemoteError>>().await,
        }
    };
    tokio::pin!(fallback);
    let mut fallback_done = false;
    let deadline = tokio::time::sleep(path_window);
    tokio::pin!(deadline);
    let mut direct_done = false;

    loop {
        tokio::select! {
            result = &mut direct, if !direct_done => {
                match result {
                    Ok(stream) => return Ok(HostBootstrapChannel::Direct(stream)),
                    Err(_) => direct_done = true,
                }
            }
            result = &mut fallback, if !fallback_done => {
                fallback_done = true;
                if let Ok(channel) = result {
                    return Ok(channel);
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    continue;
                };
                let ConnectorDiscoveryEvent::Candidate(candidate) = event else {
                    continue;
                };
                if let Ok(channel) = connect_bootstrap_via_candidate(
                    outer,
                    candidate.addr,
                    controller,
                    site_id,
                    bootstrap_key,
                ).await {
                    return Ok(channel);
                }
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
    let direct = outer.connect(controller_endpoint);
    tokio::pin!(direct);
    let discovery =
        MdnsConnectorDiscovery::attach(outer, controller.controller_id, site_id, false)?;
    let mut events = discovery.subscribe().await;
    let fallback_candidate = layout
        .and_then(|layout| {
            NearbyConnectorStore::new(layout.clone())
                .load_import(&controller, site_id)
                .ok()
                .flatten()
        })
        .map(|file| file.connector_candidate().addr);
    let fallback = async {
        match fallback_candidate {
            Some(candidate) => {
                connect_member_via_candidate(outer, candidate, controller, site_id).await
            }
            None => {
                std::future::pending::<Result<clew_transport::IrohStream, HostRemoteError>>().await
            }
        }
    };
    tokio::pin!(fallback);
    let mut fallback_done = false;
    let deadline = tokio::time::sleep(ACTIVE_MEMBER_PATH_WINDOW);
    tokio::pin!(deadline);
    let mut direct_done = false;

    loop {
        tokio::select! {
            result = &mut direct, if !direct_done => {
                match result {
                    Ok(stream) => return Ok((stream, MemberOuterPath::Direct)),
                    Err(_) => direct_done = true,
                }
            }
            result = &mut fallback, if !fallback_done => {
                fallback_done = true;
                if let Ok(stream) = result {
                    return Ok((stream, MemberOuterPath::Connector));
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    continue;
                };
                let ConnectorDiscoveryEvent::Candidate(candidate) = event else {
                    continue;
                };
                if let Ok(stream) = connect_member_via_candidate(
                    outer,
                    candidate.addr,
                    controller,
                    site_id,
                ).await {
                    return Ok((stream, MemberOuterPath::Connector));
                }
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
    layout: Option<&StateLayout>,
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

    let connector =
        membership.device.capabilities.connector && outer_path == MemberOuterPath::Direct;
    let mut connector_discovery = None;
    let mut connector_lease = None;
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
        if let Some(layout) = layout
            && let Ok(file) = NearbyConnectorFile::from_helper(outer.addr(), lease.clone())
        {
            let _ = NearbyConnectorStore::new(layout.clone()).save_export(
                &file,
                &membership.marker.controller,
                membership.marker.site_id,
            );
        }
        connector_lease = Some(lease);
        Instant::now() + Duration::from_millis(renew_in_ms)
    } else {
        Instant::now() + Duration::from_secs(24 * 60 * 60)
    };

    let mut tunnels = JoinSet::new();
    loop {
        tokio::select! {
            message = inner.recv(&mut stream) => {
                let message = message?;
                let reply = match (service.as_ref(), ReadRequest::from_message(&message)) {
                    (Some(service), Ok(request)) => service.execute(request).await,
                    (None, Ok(_)) => ReadReply::error(
                        ReadErrorCode::Denied,
                        "read is not permitted on this Connector-only device",
                    ),
                    (_, Err(_)) => ReadReply::error(
                        ReadErrorCode::InvalidRequest,
                        "unsupported or malformed v1 host request",
                    ),
                };
                inner.send(&mut stream, &reply.into_message()?).await?;
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
            _ = tokio::time::sleep_until(renew_at), if connector => {
                drop(connector_discovery.take());
                tunnels.abort_all();
                while tunnels.join_next().await.is_some() {}
                return Err(HostRemoteError::ConnectorLeaseRefreshRequired);
            }
        }
    }
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
    use clew_core::{DeviceNameOrigin, InviteId, MemberCapabilities, ReadPolicy};
    use clew_identity::{
        ControllerIdentity, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
    };
    use clew_transport::{BootstrapRequest, BootstrapResponse, noise_static_public};
    use iroh::{EndpointAddr, SecretKey};
    use tempfile::tempdir;

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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum NoPublicDiscovery {
        MdnsDelayed,
        NearbyFile,
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
            PermissionGrant {
                member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
                read: true,
                write: false,
                shell: false,
            },
        );
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Connector Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
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
        if discovery_mode == NoPublicDiscovery::NearbyFile {
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
                        PermissionGrant::EXECUTE_READ_CONNECTOR
                    }
                    BootstrapMemberMode::ConnectorOnly => PermissionGrant::CONNECTOR_ONLY,
                };
                let receipt = registry
                    .claim_with_ceiling(&pass, device_identity, now, ceiling)
                    .unwrap();
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
            source: crate::HostSiteSource::Explicit,
        };
        let (_activation_shutdown_tx, activation_shutdown_rx) = tokio::sync::watch::channel(false);
        let activation_started = Instant::now();
        let (activation_timeout, path_window) = match discovery_mode {
            NoPublicDiscovery::MdnsDelayed => (Duration::from_secs(35), Duration::from_secs(8)),
            NoPublicDiscovery::NearbyFile => (Duration::from_secs(15), Duration::from_secs(10)),
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
        let HostLaunchState::Active { membership, .. } = activated else {
            panic!("Target did not become active");
        };
        assert!(membership.device.capabilities.execute);
        assert!(membership.device.capabilities.connector);
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
                inner
                    .send(
                        &mut stream,
                        &ReadRequest::new(proof_path, 0, 4_096)
                            .unwrap()
                            .into_message()
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let reply = inner.recv(&mut stream).await.unwrap();
                assert_eq!(
                    ReadReply::from_message(&reply).unwrap(),
                    ReadReply::Data(READ_PROOF.to_vec())
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
