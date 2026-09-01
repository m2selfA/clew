use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{DeviceId, DeviceNameOrigin, DeviceRecord};
use clew_host::normalize_hostname;
use clew_identity::{EnrollmentError, StoredControllerIdentity};
use clew_transport::{
    BootstrapErrorBody, BootstrapErrorCode, BootstrapRequest, BootstrapResponse,
    ControllerSessionAuthority, InnerSession, IrohProtocol, IrohStream, ReadReply, ReadRequest,
    read_bootstrap, write_bootstrap,
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{ControlStoreError, ControllerControlStore};

pub const MAX_REMOTE_CONNECTIONS: usize = 128;
const REMOTE_COMMAND_CAPACITY: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct RemoteHub {
    inner: Arc<Mutex<RemoteHubState>>,
}

#[derive(Debug, Default)]
struct RemoteHubState {
    next_token: u64,
    sessions: BTreeMap<DeviceId, RemoteSessionSlot>,
}

#[derive(Debug)]
struct RemoteSessionSlot {
    token: u64,
    tx: mpsc::Sender<RemoteCommand>,
}

#[derive(Debug)]
enum RemoteCommand {
    Read {
        request: ReadRequest,
        reply: oneshot::Sender<Result<ReadReply, RemoteHubError>>,
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

    pub fn disconnect(&self, device_id: DeviceId) {
        if let Ok(mut state) = self.inner.lock()
            && let Some(slot) = state.sessions.remove(&device_id)
        {
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
        IrohProtocol::InnerSession => handle_member(&mut stream, &identity, control, hub).await,
    }
}

async fn handle_bootstrap(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
) -> Result<(), RemoteConnectionError> {
    let result = handle_bootstrap_inner(stream, identity, control).await;
    if let Err(error) = &result
        && let Some(body) = error.bootstrap_error_body()
    {
        send_bootstrap_rejection(stream, body).await;
    }
    result
}

async fn send_bootstrap_rejection(stream: &mut IrohStream, body: BootstrapErrorBody) {
    if write_bootstrap(stream, &BootstrapResponse::Error(body))
        .await
        .is_err()
    {
        return;
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(stream).await;
    let mut scratch = [0_u8; 1];
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::io::AsyncReadExt::read(stream, &mut scratch),
    )
    .await;
}

async fn handle_bootstrap_inner(
    stream: &mut IrohStream,
    identity: &StoredControllerIdentity,
    control: Arc<Mutex<ControllerControlStore>>,
) -> Result<(), RemoteConnectionError> {
    if remote_access_paused(&control)? {
        return Err(RemoteConnectionError::RecoveryReviewRequired);
    }
    let first: BootstrapRequest = read_bootstrap(stream).await?;
    match first {
        BootstrapRequest::Claim {
            bootstrap,
            device_identity,
            hostname,
        } => {
            validate_hostname(&hostname)?;
            let controller = bootstrap.verify()?;
            if controller != identity.public_identity() {
                return Err(RemoteConnectionError::Denied);
            }
            let site_id = bootstrap.payload.site_id;
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
                store.transaction(|snapshot| {
                    Ok(snapshot.registry.claim(&bootstrap, device_identity, now)?)
                })?
            };
            write_bootstrap(stream, &BootstrapResponse::Claimed(receipt.clone())).await?;

            let persisted: BootstrapRequest = read_bootstrap(stream).await?;
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
            write_bootstrap(stream, &BootstrapResponse::Activated(canonical)).await?;
            expect_activation_ack(stream, invite_id, device_id).await?;
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
            write_bootstrap(stream, &BootstrapResponse::Activated(canonical)).await?;
            expect_activation_ack(stream, invite_id, device_id).await?;
            Ok(())
        }
        BootstrapRequest::ActivatedAck { .. } => {
            Err(RemoteConnectionError::InvalidBootstrapSequence)
        }
    }
}

async fn expect_activation_ack(
    stream: &mut IrohStream,
    invite_id: clew_core::InviteId,
    device_id: DeviceId,
) -> Result<(), RemoteConnectionError> {
    match read_bootstrap::<BootstrapRequest, _>(stream).await? {
        BootstrapRequest::ActivatedAck {
            invite_id: actual_invite,
            device_id: actual_device,
        } if actual_invite == invite_id && actual_device == device_id => Ok(()),
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
) -> Result<(), RemoteConnectionError> {
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
            RemoteCommand::Stop => break,
        }
    }
    hub.unregister(device_id, token);
    Ok(())
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

#[derive(Debug, Error)]
pub enum RemoteHubError {
    #[error("device {0} is offline")]
    Offline(DeviceId),
    #[error("remote hub state is poisoned")]
    StatePoisoned,
    #[error("remote session token overflow")]
    TokenOverflow,
    #[error(transparent)]
    Inner(#[from] clew_transport::InnerSessionError),
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
            Self::Bootstrap(_) | Self::Inner(_) | Self::Read(_) | Self::Hub(_) => None,
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
