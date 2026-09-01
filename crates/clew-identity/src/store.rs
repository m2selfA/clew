use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use clew_core::{
    ControllerId, DeviceId, InviteId, MAX_STATE_DOCUMENT_SIZE, SiteId, StateCodecError,
    StateLayout, decode_state_json, encode_state_json,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

use crate::{
    ControllerIdentity, ControllerPublicIdentity, DeviceIdentity, DevicePublicIdentity,
    EnrollmentReceipt, IdentityError, keys::random_secret_32,
};

const CONTROLLER_NOISE_STATIC_INFO: &[u8] = b"clew/controller-noise-static/v1";
const CONTROLLER_IROH_ENDPOINT_INFO: &[u8] = b"clew/controller-iroh-endpoint/v1";

#[derive(Clone)]
pub struct StoredControllerIdentity {
    identity: ControllerIdentity,
    transport_identity_secret: [u8; 32],
}

impl std::fmt::Debug for StoredControllerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredControllerIdentity")
            .field("controller_id", &self.identity.controller_id())
            .field("controller_secret", &"[REDACTED]")
            .field("transport_identity_secret", &"[REDACTED]")
            .finish()
    }
}

impl StoredControllerIdentity {
    #[must_use]
    pub fn identity(&self) -> &ControllerIdentity {
        &self.identity
    }

    #[must_use]
    pub fn public_identity(&self) -> ControllerPublicIdentity {
        self.identity.public_identity()
    }

    #[must_use]
    pub fn noise_static_secret(&self) -> [u8; 32] {
        derive_transport_key(
            &self.transport_identity_secret,
            CONTROLLER_NOISE_STATIC_INFO,
        )
    }

    #[must_use]
    pub fn iroh_endpoint_secret(&self) -> [u8; 32] {
        derive_transport_key(
            &self.transport_identity_secret,
            CONTROLLER_IROH_ENDPOINT_INFO,
        )
    }

    pub(crate) fn transport_identity_seed(&self) -> [u8; 32] {
        self.transport_identity_secret
    }
}

fn derive_transport_key(seed: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(None, seed);
    let mut key = [0_u8; 32];
    hkdf.expand(info, &mut key)
        .expect("32-byte HKDF expansion is valid for SHA-256");
    key
}

#[derive(Clone, Debug)]
pub struct ControllerIdentityStore {
    layout: StateLayout,
}

impl ControllerIdentityStore {
    #[must_use]
    pub fn new(layout: StateLayout) -> Self {
        Self { layout }
    }

    pub fn load_or_create(&self) -> Result<StoredControllerIdentity, DeviceIdentityStoreError> {
        if let Some(existing) = self.load()? {
            return Ok(existing);
        }
        let identity = ControllerIdentity::generate()?;
        let stored = StoredControllerRecord {
            controller: identity.public_identity(),
            controller_secret_key: identity.secret_bytes(),
            transport_identity_secret: random_secret_32()?,
        };
        let path = self.layout.controller_state_path();
        match write_secret_state_create_new(&path, &stored) {
            Ok(()) => Ok(StoredControllerIdentity {
                identity,
                transport_identity_secret: stored.transport_identity_secret,
            }),
            Err(DeviceIdentityStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                self.load()?.ok_or(DeviceIdentityStoreError::LostCreateRace)
            }
            Err(error) => Err(error),
        }
    }

    pub fn restore_empty(
        &self,
        identity: ControllerIdentity,
        transport_identity_secret: [u8; 32],
    ) -> Result<StoredControllerIdentity, DeviceIdentityStoreError> {
        if self.load()?.is_some() {
            return Err(DeviceIdentityStoreError::ControllerStateNotEmpty);
        }
        let stored = StoredControllerRecord {
            controller: identity.public_identity(),
            controller_secret_key: identity.secret_bytes(),
            transport_identity_secret,
        };
        write_secret_state_create_new(&self.layout.controller_state_path(), &stored)?;
        Ok(StoredControllerIdentity {
            identity,
            transport_identity_secret,
        })
    }

    pub fn load(&self) -> Result<Option<StoredControllerIdentity>, DeviceIdentityStoreError> {
        let Some(record) =
            read_secret_state::<StoredControllerRecord>(&self.layout.controller_state_path())?
        else {
            return Ok(None);
        };
        record.controller.validate()?;
        let identity = ControllerIdentity::from_secret(record.controller_secret_key);
        if identity.public_identity() != record.controller {
            return Err(DeviceIdentityStoreError::KeyPinMismatch);
        }
        Ok(Some(StoredControllerIdentity {
            identity,
            transport_identity_secret: record.transport_identity_secret,
        }))
    }
}

#[derive(Clone)]
pub struct PendingDeviceIdentity {
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    invite_id: InviteId,
    identity: DeviceIdentity,
}

impl std::fmt::Debug for PendingDeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingDeviceIdentity")
            .field("controller", &self.controller)
            .field("site_id", &self.site_id)
            .field("invite_id", &self.invite_id)
            .field("device_public_identity", &self.identity.public_identity())
            .field("device_secret", &"[REDACTED]")
            .finish()
    }
}

impl PendingDeviceIdentity {
    #[must_use]
    pub const fn controller(&self) -> ControllerPublicIdentity {
        self.controller
    }

    #[must_use]
    pub const fn site_id(&self) -> SiteId {
        self.site_id
    }

    #[must_use]
    pub const fn invite_id(&self) -> InviteId {
        self.invite_id
    }

    #[must_use]
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    #[must_use]
    pub fn public_identity(&self) -> DevicePublicIdentity {
        self.identity.public_identity()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingControllerActivation {
    controller_id: ControllerId,
    site_id: SiteId,
    invite_id: InviteId,
    device_id: DeviceId,
    persist_ack_token: [u8; 32],
}

impl std::fmt::Debug for PendingControllerActivation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingControllerActivation")
            .field("controller_id", &self.controller_id)
            .field("site_id", &self.site_id)
            .field("invite_id", &self.invite_id)
            .field("device_id", &self.device_id)
            .field("persist_ack_token", &"[REDACTED]")
            .finish()
    }
}

impl PendingControllerActivation {
    #[must_use]
    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    #[must_use]
    pub const fn site_id(&self) -> SiteId {
        self.site_id
    }

    #[must_use]
    pub const fn invite_id(&self) -> InviteId {
        self.invite_id
    }

    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    #[must_use]
    pub const fn persist_ack_token(&self) -> &[u8; 32] {
        &self.persist_ack_token
    }
}

#[derive(Clone)]
pub struct ActiveDeviceIdentity {
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    invite_id: InviteId,
    device_id: DeviceId,
    identity: DeviceIdentity,
}

impl std::fmt::Debug for ActiveDeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveDeviceIdentity")
            .field("controller", &self.controller)
            .field("site_id", &self.site_id)
            .field("invite_id", &self.invite_id)
            .field("device_id", &self.device_id)
            .field("device_public_identity", &self.identity.public_identity())
            .field("device_secret", &"[REDACTED]")
            .finish()
    }
}

impl ActiveDeviceIdentity {
    #[must_use]
    pub const fn controller(&self) -> ControllerPublicIdentity {
        self.controller
    }

    #[must_use]
    pub const fn site_id(&self) -> SiteId {
        self.site_id
    }

    #[must_use]
    pub const fn invite_id(&self) -> InviteId {
        self.invite_id
    }

    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    #[must_use]
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    #[must_use]
    pub fn public_identity(&self) -> DevicePublicIdentity {
        self.identity.public_identity()
    }
}

#[derive(Clone, Debug)]
pub struct DeviceIdentityStore {
    layout: StateLayout,
}

impl DeviceIdentityStore {
    #[must_use]
    pub fn new(layout: StateLayout) -> Self {
        Self { layout }
    }

    pub fn prepare_pending(
        &self,
        controller: ControllerPublicIdentity,
        site_id: SiteId,
        invite_id: InviteId,
    ) -> Result<PendingDeviceIdentity, DeviceIdentityStoreError> {
        controller.validate()?;
        if let Some(active) = self.load_active(controller.controller_id, site_id)? {
            return Err(DeviceIdentityStoreError::AlreadyActive(active.device_id));
        }
        if let Some(existing) = self.load_pending(controller.controller_id, site_id)? {
            if existing.controller != controller || existing.invite_id != invite_id {
                return Err(DeviceIdentityStoreError::ScopeConflict);
            }
            return Ok(existing);
        }

        let identity = DeviceIdentity::generate()?;
        let record = PendingRecord {
            controller,
            site_id,
            invite_id,
            device_public_identity: identity.public_identity(),
            device_secret_key: identity.secret_bytes(),
        };
        let path = self
            .layout
            .pending_device_identity_path(controller.controller_id, site_id);
        match write_secret_state_create_new(&path, &record) {
            Ok(()) => Ok(PendingDeviceIdentity {
                controller,
                site_id,
                invite_id,
                identity,
            }),
            Err(DeviceIdentityStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let existing = self
                    .load_pending(controller.controller_id, site_id)?
                    .ok_or(DeviceIdentityStoreError::LostCreateRace)?;
                if existing.controller != controller || existing.invite_id != invite_id {
                    return Err(DeviceIdentityStoreError::ScopeConflict);
                }
                Ok(existing)
            }
            Err(error) => Err(error),
        }
    }

    pub fn load_pending(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> Result<Option<PendingDeviceIdentity>, DeviceIdentityStoreError> {
        let path = self
            .layout
            .pending_device_identity_path(controller_id, site_id);
        let Some(record) = read_secret_state::<PendingRecord>(&path)? else {
            return Ok(None);
        };
        if record.controller.controller_id != controller_id || record.site_id != site_id {
            return Err(DeviceIdentityStoreError::ScopeConflict);
        }
        record.controller.validate()?;
        record.device_public_identity.validate()?;
        let identity = DeviceIdentity::from_secret(record.device_secret_key);
        if identity.public_identity() != record.device_public_identity {
            return Err(DeviceIdentityStoreError::KeyPinMismatch);
        }
        Ok(Some(PendingDeviceIdentity {
            controller: record.controller,
            site_id: record.site_id,
            invite_id: record.invite_id,
            identity,
        }))
    }

    pub fn load_pending_controller_activation(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> Result<Option<PendingControllerActivation>, DeviceIdentityStoreError> {
        let path = self
            .layout
            .pending_controller_activation_path(controller_id, site_id);
        let Some(record) = read_secret_state::<PendingControllerActivation>(&path)? else {
            return Ok(None);
        };
        if record.controller_id != controller_id || record.site_id != site_id {
            return Err(DeviceIdentityStoreError::ScopeConflict);
        }
        Ok(Some(record))
    }

    pub fn load_active(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> Result<Option<ActiveDeviceIdentity>, DeviceIdentityStoreError> {
        let path = self
            .layout
            .active_device_identity_path(controller_id, site_id);
        let Some(record) = read_secret_state::<ActiveRecord>(&path)? else {
            return Ok(None);
        };
        if record.controller.controller_id != controller_id || record.site_id != site_id {
            return Err(DeviceIdentityStoreError::ScopeConflict);
        }
        record.controller.validate()?;
        record.device_public_identity.validate()?;
        let identity = DeviceIdentity::from_secret(record.device_secret_key);
        if identity.public_identity() != record.device_public_identity {
            return Err(DeviceIdentityStoreError::KeyPinMismatch);
        }
        Ok(Some(ActiveDeviceIdentity {
            controller: record.controller,
            site_id: record.site_id,
            invite_id: record.invite_id,
            device_id: record.device_id,
            identity,
        }))
    }

    pub fn promote_active(
        &self,
        pending: &PendingDeviceIdentity,
        receipt: &EnrollmentReceipt,
    ) -> Result<ActiveDeviceIdentity, DeviceIdentityStoreError> {
        if receipt.controller_id != pending.controller.controller_id
            || receipt.site_id != pending.site_id
            || receipt.invite_id != pending.invite_id
            || receipt.device_public_identity != pending.public_identity()
        {
            return Err(DeviceIdentityStoreError::ReceiptMismatch);
        }
        if let Some(active) = self.load_active(receipt.controller_id, receipt.site_id)? {
            if active.device_id == receipt.device_id
                && active.public_identity() == pending.public_identity()
            {
                return Ok(active);
            }
            return Err(DeviceIdentityStoreError::ActiveConflict);
        }

        let record = ActiveRecord {
            controller: pending.controller,
            site_id: pending.site_id,
            invite_id: pending.invite_id,
            device_id: receipt.device_id,
            device_public_identity: pending.public_identity(),
            device_secret_key: pending.identity.secret_bytes(),
        };
        let active_path = self
            .layout
            .active_device_identity_path(receipt.controller_id, receipt.site_id);
        match write_secret_state_create_new(&active_path, &record) {
            Ok(()) => {}
            Err(DeviceIdentityStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let active = self
                    .load_active(receipt.controller_id, receipt.site_id)?
                    .ok_or(DeviceIdentityStoreError::LostCreateRace)?;
                if active.device_id != receipt.device_id
                    || active.public_identity() != pending.public_identity()
                {
                    return Err(DeviceIdentityStoreError::ActiveConflict);
                }
                return Ok(active);
            }
            Err(error) => return Err(error),
        }

        let pending_path = self
            .layout
            .pending_device_identity_path(receipt.controller_id, receipt.site_id);
        match fs::remove_file(&pending_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DeviceIdentityStoreError::Io(error)),
        }
        Ok(ActiveDeviceIdentity {
            controller: pending.controller,
            site_id: pending.site_id,
            invite_id: pending.invite_id,
            device_id: receipt.device_id,
            identity: pending.identity.clone(),
        })
    }
    pub fn promote_active_pending_controller_ack(
        &self,
        pending: &PendingDeviceIdentity,
        receipt: &EnrollmentReceipt,
    ) -> Result<ActiveDeviceIdentity, DeviceIdentityStoreError> {
        if receipt.controller_id != pending.controller.controller_id
            || receipt.site_id != pending.site_id
            || receipt.invite_id != pending.invite_id
            || receipt.device_public_identity != pending.public_identity()
        {
            return Err(DeviceIdentityStoreError::ReceiptMismatch);
        }
        let activation = PendingControllerActivation {
            controller_id: receipt.controller_id,
            site_id: receipt.site_id,
            invite_id: receipt.invite_id,
            device_id: receipt.device_id,
            persist_ack_token: *receipt.persist_ack_token(),
        };
        let path = self
            .layout
            .pending_controller_activation_path(receipt.controller_id, receipt.site_id);
        match write_secret_state_create_new(&path, &activation) {
            Ok(()) => {}
            Err(DeviceIdentityStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let existing = self
                    .load_pending_controller_activation(receipt.controller_id, receipt.site_id)?
                    .ok_or(DeviceIdentityStoreError::LostCreateRace)?;
                if existing != activation {
                    return Err(DeviceIdentityStoreError::ActivationConflict);
                }
            }
            Err(error) => return Err(error),
        }
        self.promote_active(pending, receipt)
    }

    pub fn confirm_controller_activation(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> Result<(), DeviceIdentityStoreError> {
        let Some(pending) = self.load_pending_controller_activation(controller_id, site_id)? else {
            return Ok(());
        };
        if pending.device_id != device_id {
            return Err(DeviceIdentityStoreError::ActivationConflict);
        }
        let path = self
            .layout
            .pending_controller_activation_path(controller_id, site_id);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredControllerRecord {
    controller: ControllerPublicIdentity,
    controller_secret_key: [u8; 32],
    transport_identity_secret: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct PendingRecord {
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    invite_id: InviteId,
    device_public_identity: DevicePublicIdentity,
    device_secret_key: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct ActiveRecord {
    controller: ControllerPublicIdentity,
    site_id: SiteId,
    invite_id: InviteId,
    device_id: DeviceId,
    device_public_identity: DevicePublicIdentity,
    device_secret_key: [u8; 32],
}

fn read_secret_state<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, DeviceIdentityStoreError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(DeviceIdentityStoreError::Io(error)),
    };
    if metadata.len() > MAX_STATE_DOCUMENT_SIZE as u64 {
        return Err(DeviceIdentityStoreError::DocumentTooLarge(metadata.len()));
    }
    let encoded = fs::read(path)?;
    Ok(Some(decode_state_json(&encoded)?))
}

fn write_secret_state_create_new<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), DeviceIdentityStoreError> {
    let parent = path
        .parent()
        .ok_or(DeviceIdentityStoreError::InvalidStatePath)?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let encoded = encode_state_json(value)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum DeviceIdentityStoreError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    State(#[from] StateCodecError),
    #[error("device identity I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("device identity document is too large: {0} bytes")]
    DocumentTooLarge(u64),
    #[error("device identity state path has no parent")]
    InvalidStatePath,
    #[error("Controller identity restore requires an empty Controller state")]
    ControllerStateNotEmpty,
    #[error("stored private key does not match its persisted public identity pin")]
    KeyPinMismatch,
    #[error("device identity state belongs to a different Controller/Site/invite")]
    ScopeConflict,
    #[error("an active device identity already exists for this Controller/Site: {0}")]
    AlreadyActive(DeviceId),
    #[error("enrollment receipt does not match the pending DeviceKey scope")]
    ReceiptMismatch,
    #[error("active device identity conflicts with the enrollment receipt")]
    ActiveConflict,
    #[error("pending Controller activation conflicts with the persisted device identity")]
    ActivationConflict,
    #[error("device identity create race completed without a readable winner")]
    LostCreateRace,
}

#[cfg(test)]
mod tests {
    use clew_core::MemberCapabilities;
    use tempfile::tempdir;

    use super::*;
    use crate::{EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec};

    #[test]
    fn controller_transport_seed_derives_stable_separated_noise_and_iroh_keys() {
        let temp = tempdir().unwrap();
        let store = ControllerIdentityStore::new(StateLayout::new(temp.path()));
        let first = store.load_or_create().unwrap();
        let reopened = store.load().unwrap().unwrap();
        assert_eq!(first.noise_static_secret(), reopened.noise_static_secret());
        assert_eq!(
            first.iroh_endpoint_secret(),
            reopened.iroh_endpoint_secret()
        );
        assert_ne!(first.noise_static_secret(), first.iroh_endpoint_secret());
        assert_ne!(first.noise_static_secret(), first.transport_identity_seed());
        assert_ne!(
            first.iroh_endpoint_secret(),
            first.transport_identity_seed()
        );
    }

    #[test]
    fn controller_activation_ack_survives_host_persist_gap_until_explicit_confirmation() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let store = DeviceIdentityStore::new(layout.clone());
        let controller = crate::ControllerIdentity::from_secret([40_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let pending = store
            .prepare_pending(controller.public_identity(), site_id, invite_id)
            .unwrap();
        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant {
                member: MemberCapabilities::EXECUTE_ONLY,
                read: true,
                write: false,
                shell: false,
            },
        );
        let pass = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 100,
                    deployment_window_ms: 50,
                    max_claims: 1,
                },
            )
            .unwrap();
        let receipt = registry
            .claim(&pass, pending.public_identity(), 10)
            .unwrap();
        let active = store
            .promote_active_pending_controller_ack(&pending, &receipt)
            .unwrap();
        let activation = store
            .load_pending_controller_activation(controller.controller_id(), site_id)
            .unwrap()
            .unwrap();
        assert_eq!(activation.device_id(), active.device_id());
        assert_eq!(activation.persist_ack_token(), receipt.persist_ack_token());

        store
            .confirm_controller_activation(controller.controller_id(), site_id, active.device_id())
            .unwrap();
        assert!(
            store
                .load_pending_controller_activation(controller.controller_id(), site_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn corrupted_controller_secret_cannot_silently_change_controller_pin() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let store = ControllerIdentityStore::new(layout.clone());
        let original = store.load_or_create().unwrap();
        let encoded = fs::read(layout.controller_state_path()).unwrap();
        let mut record: StoredControllerRecord = decode_state_json(&encoded).unwrap();
        record.controller_secret_key[0] ^= 1;
        fs::write(
            layout.controller_state_path(),
            encode_state_json(&record).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            store.load(),
            Err(DeviceIdentityStoreError::KeyPinMismatch)
        ));
        assert_eq!(
            original.public_identity().controller_id,
            record.controller.controller_id
        );
    }

    #[test]
    fn pending_key_survives_controller_registered_host_persist_gap() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let store = DeviceIdentityStore::new(layout);
        let controller = crate::ControllerIdentity::from_secret([41_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let pending = store
            .prepare_pending(controller.public_identity(), site_id, invite_id)
            .unwrap();
        let original_public = pending.public_identity();

        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant {
                member: MemberCapabilities::EXECUTE_ONLY,
                read: true,
                write: false,
                shell: false,
            },
        );
        let pass = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 100,
                    deployment_window_ms: 50,
                    max_claims: 1,
                },
            )
            .unwrap();
        let receipt = registry.claim(&pass, original_public, 10).unwrap();

        // Simulate a process restart before the Host could persist DeviceId.
        let recovered_pending = store
            .load_pending(controller.controller_id(), site_id)
            .unwrap()
            .unwrap();
        assert_eq!(recovered_pending.public_identity(), original_public);
        let recovered_receipt = registry
            .claim(&pass, recovered_pending.public_identity(), 1_000)
            .unwrap();
        assert_eq!(recovered_receipt.device_id, receipt.device_id);

        let active = store
            .promote_active(&recovered_pending, &recovered_receipt)
            .unwrap();
        registry
            .finalize_host_persist(
                invite_id,
                active.device_id(),
                recovered_receipt.persist_ack_token(),
            )
            .unwrap();
        assert!(
            store
                .load_pending(controller.controller_id(), site_id)
                .unwrap()
                .is_none()
        );
        let reloaded = store
            .load_active(controller.controller_id(), site_id)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.device_id(), active.device_id());
        assert_eq!(reloaded.public_identity(), original_public);
    }
}
