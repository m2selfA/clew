use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use clew_core::{
    ControllerId, DeviceNameOrigin, DeviceRecord, InviteId, MAX_STATE_DOCUMENT_SIZE, SiteId,
    StateCodecError, StateLayout, decode_state_json, encode_state_json,
};
use clew_identity::{
    ActiveDeviceIdentity, ControllerPublicIdentity, DeviceIdentityStore, DeviceIdentityStoreError,
    EnrollmentReceipt, PendingDeviceIdentity,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ClientFlavorId, normalize_hostname};

const MAX_MEMBERSHIP_SCAN_ENTRIES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostMembershipMarker {
    pub client_flavor_id: ClientFlavorId,
    pub controller: ControllerPublicIdentity,
    pub site_id: SiteId,
    pub site_name: String,
    pub device_id: clew_core::DeviceId,
    pub invite_id: InviteId,
}

#[derive(Clone, Debug)]
pub struct HostMembership {
    pub marker: HostMembershipMarker,
    pub device: DeviceRecord,
    pub identity: ActiveDeviceIdentity,
}

#[derive(Clone, Debug)]
pub struct HostMembershipStore {
    layout: StateLayout,
}

impl HostMembershipStore {
    #[must_use]
    pub fn new(layout: StateLayout) -> Self {
        Self { layout }
    }

    pub fn activate(
        &self,
        client_flavor_id: ClientFlavorId,
        site_name: &str,
        pending: &PendingDeviceIdentity,
        receipt: &EnrollmentReceipt,
        hostname: &str,
    ) -> Result<HostMembership, HostMembershipError> {
        if receipt.controller_id != pending.controller().controller_id
            || receipt.site_id != pending.site_id()
            || receipt.invite_id != pending.invite_id()
            || receipt.device_public_identity != pending.public_identity()
        {
            return Err(HostMembershipError::ReceiptMismatch);
        }
        let site_name = validate_site_name(site_name)?;
        let base_hostname = normalize_hostname(hostname);
        let record = DeviceRecord {
            device_id: receipt.device_id,
            site_id: receipt.site_id,
            display_name: base_hostname.clone(),
            hostname_observed: base_hostname.clone(),
            capabilities: receipt.effective_grant.member,
            enrolled_via_invite_id: receipt.invite_id,
            name_origin: DeviceNameOrigin::Automatic {
                base_hostname,
                tagged: false,
                tag_generation: 0,
            },
        };
        write_or_verify_state(
            &self.layout.device_record_path(
                receipt.controller_id,
                receipt.site_id,
                receipt.device_id,
            ),
            &record,
        )?;
        let identity_store = DeviceIdentityStore::new(self.layout.clone());
        let identity = identity_store.promote_active(pending, receipt)?;
        let marker = HostMembershipMarker {
            client_flavor_id,
            controller: pending.controller(),
            site_id: receipt.site_id,
            site_name,
            device_id: receipt.device_id,
            invite_id: receipt.invite_id,
        };
        write_or_verify_state(
            &self
                .layout
                .host_membership_marker_path(receipt.controller_id, receipt.site_id),
            &marker,
        )?;
        Ok(HostMembership {
            marker,
            device: record,
            identity,
        })
    }

    pub fn load(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> Result<Option<HostMembership>, HostMembershipError> {
        let marker_path = self
            .layout
            .host_membership_marker_path(controller_id, site_id);
        let Some(marker) = read_state_if_exists::<HostMembershipMarker>(&marker_path)? else {
            return Ok(None);
        };
        validate_marker_scope(&marker, controller_id, site_id)?;
        let identity = DeviceIdentityStore::new(self.layout.clone())
            .load_active(controller_id, site_id)?
            .ok_or(HostMembershipError::IncompleteMembership)?;
        if identity.device_id() != marker.device_id
            || identity.controller() != marker.controller
            || identity.invite_id() != marker.invite_id
        {
            return Err(HostMembershipError::MarkerMismatch);
        }
        let device = read_state_required::<DeviceRecord>(&self.layout.device_record_path(
            controller_id,
            site_id,
            marker.device_id,
        ))?;
        if device.device_id != marker.device_id
            || device.site_id != marker.site_id
            || device.enrolled_via_invite_id != marker.invite_id
        {
            return Err(HostMembershipError::MarkerMismatch);
        }
        Ok(Some(HostMembership {
            marker,
            device,
            identity,
        }))
    }

    pub fn recover_active_from_site(
        &self,
        client_flavor_id: ClientFlavorId,
        controller: ControllerPublicIdentity,
        site_id: SiteId,
        site_name: &str,
    ) -> Result<Option<HostMembership>, HostMembershipError> {
        controller.validate()?;
        let identity_store = DeviceIdentityStore::new(self.layout.clone());
        let Some(identity) = identity_store.load_active(controller.controller_id, site_id)? else {
            return Ok(None);
        };
        if identity.controller() != controller {
            return Err(HostMembershipError::MarkerMismatch);
        }
        let device = read_state_required::<DeviceRecord>(&self.layout.device_record_path(
            controller.controller_id,
            site_id,
            identity.device_id(),
        ))?;
        if device.device_id != identity.device_id()
            || device.site_id != site_id
            || device.enrolled_via_invite_id != identity.invite_id()
        {
            return Err(HostMembershipError::MarkerMismatch);
        }
        let marker = HostMembershipMarker {
            client_flavor_id,
            controller,
            site_id,
            site_name: validate_site_name(site_name)?,
            device_id: identity.device_id(),
            invite_id: identity.invite_id(),
        };
        write_or_verify_state(
            &self
                .layout
                .host_membership_marker_path(controller.controller_id, site_id),
            &marker,
        )?;
        Ok(Some(HostMembership {
            marker,
            device,
            identity,
        }))
    }

    pub fn recover_for_flavor(
        &self,
        client_flavor_id: ClientFlavorId,
    ) -> Result<Vec<HostMembership>, HostMembershipError> {
        let root = self.layout.version_root().join("memberships");
        let controllers = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut scanned = 0_usize;
        let mut found = Vec::new();
        for controller_entry in controllers {
            let controller_entry = controller_entry?;
            if !controller_entry.file_type()?.is_dir() {
                continue;
            }
            scanned += 1;
            check_scan_bound(scanned)?;
            let Ok(controller_id) = controller_entry.file_name().to_string_lossy().parse() else {
                continue;
            };
            for site_entry in fs::read_dir(controller_entry.path())? {
                let site_entry = site_entry?;
                if !site_entry.file_type()?.is_dir() {
                    continue;
                }
                scanned += 1;
                check_scan_bound(scanned)?;
                let Ok(site_id) = site_entry.file_name().to_string_lossy().parse() else {
                    continue;
                };
                let marker_path = self
                    .layout
                    .host_membership_marker_path(controller_id, site_id);
                let Some(marker) = read_state_if_exists::<HostMembershipMarker>(&marker_path)?
                else {
                    continue;
                };
                if marker.client_flavor_id != client_flavor_id {
                    continue;
                }
                let membership = self
                    .load(controller_id, site_id)?
                    .ok_or(HostMembershipError::IncompleteMembership)?;
                found.push(membership);
            }
        }
        found.sort_by(|left, right| {
            left.marker
                .site_name
                .cmp(&right.marker.site_name)
                .then_with(|| {
                    left.marker
                        .controller
                        .controller_id
                        .to_string()
                        .cmp(&right.marker.controller.controller_id.to_string())
                })
                .then_with(|| {
                    left.marker
                        .site_id
                        .to_string()
                        .cmp(&right.marker.site_id.to_string())
                })
        });
        Ok(found)
    }
}

fn validate_marker_scope(
    marker: &HostMembershipMarker,
    controller_id: ControllerId,
    site_id: SiteId,
) -> Result<(), HostMembershipError> {
    marker.controller.validate()?;
    if marker.controller.controller_id != controller_id || marker.site_id != site_id {
        return Err(HostMembershipError::MarkerMismatch);
    }
    validate_site_name(&marker.site_name)?;
    Ok(())
}

fn validate_site_name(value: &str) -> Result<String, HostMembershipError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(HostMembershipError::InvalidSiteName);
    }
    Ok(trimmed.into())
}

fn check_scan_bound(scanned: usize) -> Result<(), HostMembershipError> {
    if scanned > MAX_MEMBERSHIP_SCAN_ENTRIES {
        return Err(HostMembershipError::TooManyMembershipEntries);
    }
    Ok(())
}

fn read_state_required<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<T, HostMembershipError> {
    read_state_if_exists(path)?.ok_or(HostMembershipError::IncompleteMembership)
}

fn read_state_if_exists<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, HostMembershipError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > MAX_STATE_DOCUMENT_SIZE as u64 {
        return Err(HostMembershipError::DocumentTooLarge(metadata.len()));
    }
    let encoded = fs::read(path)?;
    Ok(Some(decode_state_json(&encoded)?))
}

fn write_or_verify_state<T>(path: &Path, value: &T) -> Result<(), HostMembershipError>
where
    T: Serialize + for<'de> Deserialize<'de> + Eq,
{
    let parent = path.parent().ok_or(HostMembershipError::InvalidStatePath)?;
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
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&encoded)?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: T = read_state_required(path)?;
            if &existing == value {
                Ok(())
            } else {
                Err(HostMembershipError::StateConflict)
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Error)]
pub enum HostMembershipError {
    #[error("host membership I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] StateCodecError),
    #[error(transparent)]
    Identity(#[from] clew_identity::IdentityError),
    #[error(transparent)]
    IdentityStore(#[from] DeviceIdentityStoreError),
    #[error("enrollment receipt does not match pending host identity")]
    ReceiptMismatch,
    #[error("host membership marker does not match identity/device state")]
    MarkerMismatch,
    #[error("host membership is only partially persisted")]
    IncompleteMembership,
    #[error("host membership state conflicts with an existing record")]
    StateConflict,
    #[error("host membership state path has no parent")]
    InvalidStatePath,
    #[error("host membership document is too large: {0} bytes")]
    DocumentTooLarge(u64),
    #[error("host membership scan exceeded the bounded 256-entry state index")]
    TooManyMembershipEntries,
    #[error("site name must be 1..=128 UTF-8 bytes")]
    InvalidSiteName,
}

#[cfg(test)]
mod tests {
    use clew_core::{InviteId, MemberCapabilities};
    use clew_identity::{
        ControllerIdentity, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::ClientFlavor;

    #[test]
    fn active_membership_reuses_device_id_and_is_recoverable_without_sidecar() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller = ControllerIdentity::from_secret([81_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let identity_store = DeviceIdentityStore::new(layout.clone());
        let pending = identity_store
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
                    site_name: "Alice Lab".into(),
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
        let flavor_id = ClientFlavor::clew_original_current().id().unwrap();
        let store = HostMembershipStore::new(layout.clone());
        let active = store
            .activate(flavor_id, "Alice Lab", &pending, &receipt, "GPU-01")
            .unwrap();
        registry
            .finalize_host_persist(invite_id, receipt.device_id, receipt.persist_ack_token())
            .unwrap();

        let reopened = store
            .load(controller.controller_id(), site_id)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.marker.device_id, active.marker.device_id);
        assert_eq!(reopened.identity.device_id(), active.identity.device_id());
        let recovered = store.recover_for_flavor(flavor_id).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].marker.device_id, receipt.device_id);
    }
}
