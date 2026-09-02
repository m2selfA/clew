use std::{future::Future, time::Duration};

use clew_core::{DeviceRecord, StateLayout};
use clew_identity::DeviceIdentityStore;
use clew_transport::{
    BootstrapErrorCode, BootstrapRequest, BootstrapResponse, DeviceSessionIdentity, InnerSession,
    IrohOuter, ReadErrorCode, ReadReply, ReadRequest, read_bootstrap, write_bootstrap,
};
use thiserror::Error;

use crate::{HostLaunchState, HostMembership, HostMembershipStore, HostReadService};

pub async fn complete_networked_activation(
    layout: &StateLayout,
    state: HostLaunchState,
) -> Result<HostLaunchState, HostRemoteError> {
    match state {
        HostLaunchState::AwaitingEnrollment {
            site_file,
            pending,
            hostname,
            source,
        } => {
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
            let outer = bind_online_outer().await?;
            let mut stream = outer.connect_bootstrap(endpoint.clone()).await?;
            write_bootstrap(
                &mut stream,
                &BootstrapRequest::Claim {
                    bootstrap: site_file.payload.bootstrap.clone(),
                    device_identity: pending.public_identity(),
                    hostname: hostname.clone(),
                },
            )
            .await?;
            let receipt = match read_bootstrap::<BootstrapResponse, _>(&mut stream).await? {
                BootstrapResponse::Claimed(receipt) => receipt,
                BootstrapResponse::Error(error) => {
                    return Err(HostRemoteError::BootstrapRejected {
                        code: error.code,
                        message: error.message,
                    });
                }
                BootstrapResponse::Activated(_) => {
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
            )?;
            write_bootstrap(
                &mut stream,
                &BootstrapRequest::Persisted {
                    invite_id: receipt.invite_id,
                    device_id: receipt.device_id,
                    persist_ack_token: *receipt.persist_ack_token(),
                    hostname: hostname.clone(),
                },
            )
            .await?;
            let activated = expect_activated(read_bootstrap(&mut stream).await?)?;
            verify_activated(&membership, &activated)?;
            write_bootstrap(
                &mut stream,
                &BootstrapRequest::ActivatedAck {
                    invite_id: receipt.invite_id,
                    device_id: receipt.device_id,
                },
            )
            .await?;
            DeviceIdentityStore::new(layout.clone()).confirm_controller_activation(
                membership.marker.controller.controller_id,
                membership.marker.site_id,
                membership.marker.device_id,
            )?;
            Ok(HostLaunchState::Active { membership, source })
        }
        HostLaunchState::Active { membership, source } => {
            resume_pending_controller_activation(layout, &membership).await?;
            Ok(HostLaunchState::Active { membership, source })
        }
        other => Ok(other),
    }
}

async fn resume_pending_controller_activation(
    layout: &StateLayout,
    membership: &HostMembership,
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
    let outer = bind_online_outer().await?;
    let mut stream = outer.connect_bootstrap(endpoint).await?;
    write_bootstrap(
        &mut stream,
        &BootstrapRequest::Persisted {
            invite_id: activation.invite_id(),
            device_id: activation.device_id(),
            persist_ack_token: *activation.persist_ack_token(),
            hostname: membership.device.hostname_observed.clone(),
        },
    )
    .await?;
    let activated = expect_activated(read_bootstrap(&mut stream).await?)?;
    verify_activated(membership, &activated)?;
    write_bootstrap(
        &mut stream,
        &BootstrapRequest::ActivatedAck {
            invite_id: activation.invite_id(),
            device_id: activation.device_id(),
        },
    )
    .await?;
    identity_store.confirm_controller_activation(
        membership.marker.controller.controller_id,
        membership.marker.site_id,
        membership.marker.device_id,
    )?;
    Ok(())
}

pub async fn serve_networked_membership_until<F>(
    membership: &HostMembership,
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
        let online = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = outer.online_addr() => result,
        };
        if online.is_ok() {
            break;
        }
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }

    loop {
        let result = tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = serve_networked_membership_with_outer(
                membership,
                &outer,
                endpoint.clone(),
                &service,
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
    let outer = bind_online_outer().await?;
    serve_networked_membership_with_outer(membership, &outer, endpoint, &service).await
}

fn member_remote_config(
    membership: &HostMembership,
) -> Result<(iroh::EndpointAddr, HostReadService), HostRemoteError> {
    if !membership.device.capabilities.execute {
        return Err(HostRemoteError::ExecutionDisabled);
    }
    let endpoint = membership
        .marker
        .controller_endpoint
        .clone()
        .ok_or(HostRemoteError::MissingNetworkConfig)?;
    let policy = membership
        .marker
        .read_policy
        .clone()
        .ok_or(HostRemoteError::MissingNetworkConfig)?;
    Ok((endpoint, HostReadService::new(policy)?))
}

async fn bind_online_outer() -> Result<IrohOuter, HostRemoteError> {
    let outer = IrohOuter::bind().await?;
    outer.online_addr().await?;
    Ok(outer)
}

async fn serve_networked_membership_with_outer(
    membership: &HostMembership,
    outer: &IrohOuter,
    endpoint: iroh::EndpointAddr,
    service: &HostReadService,
) -> Result<(), HostRemoteError> {
    let mut stream = outer.connect(endpoint).await?;
    let mut inner = InnerSession::connect(
        &mut stream,
        DeviceSessionIdentity::from_active(&membership.identity),
    )
    .await?;
    loop {
        let message = inner.recv(&mut stream).await?;
        let reply = match ReadRequest::from_message(&message) {
            Ok(request) => service.execute(request).await,
            Err(_) => ReadReply::error(
                ReadErrorCode::InvalidRequest,
                "unsupported or malformed v1 host request",
            ),
        };
        inner.send(&mut stream, &reply.into_message()?).await?;
    }
}

fn expect_activated(response: BootstrapResponse) -> Result<DeviceRecord, HostRemoteError> {
    match response {
        BootstrapResponse::Activated(record) => Ok(record),
        BootstrapResponse::Error(error) => Err(HostRemoteError::BootstrapRejected {
            code: error.code,
            message: error.message,
        }),
        BootstrapResponse::Claimed(_) => Err(HostRemoteError::UnexpectedBootstrapResponse),
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
    #[error("this membership is not executable")]
    ExecutionDisabled,
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
