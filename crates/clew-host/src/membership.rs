use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use clew_core::{
    ControllerId, DeviceNameOrigin, DeviceRecord, InviteId, MAX_STATE_DOCUMENT_SIZE, ReadPolicy,
    SiteId, StateCodecError, StateLayout, decode_state_json, encode_state_json,
};
use clew_identity::{
    ActiveDeviceIdentity, ControllerPublicIdentity, DeviceIdentityStore, DeviceIdentityStoreError,
    EnrollmentReceipt, PendingDeviceIdentity, PermissionGrant,
};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ClientFlavor, ClientFlavorId, OutfitProfile, normalize_hostname};

const MAX_MEMBERSHIP_SCAN_ENTRIES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostMembershipMarker {
    pub client_flavor_id: ClientFlavorId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_flavor: Option<ClientFlavor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outfit_profile: Option<OutfitProfile>,
    pub controller: ControllerPublicIdentity,
    pub site_id: SiteId,
    pub site_name: String,
    pub device_id: clew_core::DeviceId,
    pub invite_id: InviteId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_endpoint: Option<EndpointAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_policy: Option<ReadPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_grant: Option<PermissionGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_bootstrap_noise_public_key: Option<[u8; 32]>,
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
        self.activate_with_network(
            client_flavor_id,
            None,
            None,
            site_name,
            pending,
            receipt,
            hostname,
            None,
            None,
            None,
        )
    }

    pub fn activate_networked(
        &self,
        client_flavor: ClientFlavor,
        outfit_profile: Option<OutfitProfile>,
        site_name: &str,
        pending: &PendingDeviceIdentity,
        receipt: &EnrollmentReceipt,
        hostname: &str,
        controller_endpoint: EndpointAddr,
        read_policy: ReadPolicy,
        controller_bootstrap_noise_public_key: Option<[u8; 32]>,
    ) -> Result<HostMembership, HostMembershipError> {
        read_policy.validate()?;
        validate_flavor_profile(&client_flavor, outfit_profile.as_ref())?;
        let client_flavor_id = client_flavor.id()?;
        self.activate_with_network(
            client_flavor_id,
            Some(client_flavor),
            outfit_profile,
            site_name,
            pending,
            receipt,
            hostname,
            Some(controller_endpoint),
            Some(read_policy),
            controller_bootstrap_noise_public_key,
        )
    }

    fn activate_with_network(
        &self,
        client_flavor_id: ClientFlavorId,
        client_flavor: Option<ClientFlavor>,
        outfit_profile: Option<OutfitProfile>,
        site_name: &str,
        pending: &PendingDeviceIdentity,
        receipt: &EnrollmentReceipt,
        hostname: &str,
        controller_endpoint: Option<EndpointAddr>,
        read_policy: Option<ReadPolicy>,
        controller_bootstrap_noise_public_key: Option<[u8; 32]>,
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
        let identity = if controller_endpoint.is_some() {
            identity_store.promote_active_pending_controller_ack(pending, receipt)?
        } else {
            identity_store.promote_active(pending, receipt)?
        };
        let marker = HostMembershipMarker {
            client_flavor_id,
            client_flavor,
            outfit_profile,
            controller: pending.controller(),
            site_id: receipt.site_id,
            site_name,
            device_id: receipt.device_id,
            invite_id: receipt.invite_id,
            controller_endpoint,
            read_policy,
            effective_grant: Some(receipt.effective_grant),
            controller_bootstrap_noise_public_key,
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
        client_flavor: ClientFlavor,
        outfit_profile: Option<OutfitProfile>,
        controller: ControllerPublicIdentity,
        site_id: SiteId,
        site_name: &str,
        controller_endpoint: Option<EndpointAddr>,
        read_policy: Option<ReadPolicy>,
        signed_grant: PermissionGrant,
        controller_bootstrap_noise_public_key: Option<[u8; 32]>,
    ) -> Result<Option<HostMembership>, HostMembershipError> {
        controller.validate()?;
        validate_flavor_profile(&client_flavor, outfit_profile.as_ref())?;
        let client_flavor_id = client_flavor.id()?;
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
        let grant_ceiling = if device.capabilities.execute {
            PermissionGrant::EXECUTE_READ_WRITE_CONNECTOR
        } else {
            PermissionGrant::CONNECTOR_ONLY
        };
        let effective_grant = signed_grant.intersect(grant_ceiling);
        if effective_grant.member != device.capabilities {
            return Err(HostMembershipError::MarkerMismatch);
        }
        let marker = HostMembershipMarker {
            client_flavor_id,
            client_flavor: Some(client_flavor),
            outfit_profile,
            controller,
            site_id,
            site_name: validate_site_name(site_name)?,
            device_id: identity.device_id(),
            invite_id: identity.invite_id(),
            controller_endpoint,
            read_policy,
            effective_grant: Some(effective_grant),
            controller_bootstrap_noise_public_key,
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
        self.recover_matching(|marker| Ok(marker.client_flavor_id == client_flavor_id))
    }

    pub fn recover_for_runtime(
        &self,
        runtime: &ClientFlavor,
    ) -> Result<Vec<HostMembership>, HostMembershipError> {
        self.recover_matching(|marker| marker_matches_runtime(marker, runtime))
    }

    fn recover_matching(
        &self,
        mut matches: impl FnMut(&HostMembershipMarker) -> Result<bool, HostMembershipError>,
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
                if !matches(&marker)? {
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

fn validate_flavor_profile(
    flavor: &ClientFlavor,
    profile: Option<&OutfitProfile>,
) -> Result<(), HostMembershipError> {
    if let Some(profile) = profile {
        profile.validate()?;
        if profile.outfit_id != flavor.outfit_id || profile.revision != flavor.outfit_revision {
            return Err(HostMembershipError::MarkerMismatch);
        }
    }
    Ok(())
}

fn marker_matches_runtime(
    marker: &HostMembershipMarker,
    runtime: &ClientFlavor,
) -> Result<bool, HostMembershipError> {
    let Some(stored_flavor) = &marker.client_flavor else {
        return Ok(marker.client_flavor_id == runtime.id()?);
    };
    let mut expected = runtime.clone();
    if let Some(profile) = &marker.outfit_profile {
        validate_flavor_profile(stored_flavor, Some(profile))?;
        expected.outfit_id = profile.outfit_id.clone();
        expected.outfit_revision = profile.revision;
    }
    Ok(stored_flavor == &expected && marker.client_flavor_id == expected.id()?)
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
    if let Some(flavor) = &marker.client_flavor {
        validate_flavor_profile(flavor, marker.outfit_profile.as_ref())?;
        if flavor.id()? != marker.client_flavor_id {
            return Err(HostMembershipError::MarkerMismatch);
        }
    } else if marker.outfit_profile.is_some() {
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
    Model(#[from] clew_core::ControlModelError),
    #[error(transparent)]
    Identity(#[from] clew_identity::IdentityError),
    #[error(transparent)]
    IdentityStore(#[from] DeviceIdentityStoreError),
    #[error(transparent)]
    Outfit(#[from] crate::OutfitError),
    #[error(transparent)]
    SiteClew(#[from] crate::SiteClewError),
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
    use crate::{ClientFlavor, OutfitPreset, OutfitProfile};

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
        assert_eq!(active.marker.effective_grant, Some(receipt.effective_grant));

        let mut legacy_json = serde_json::to_value(&active.marker).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("effective_grant");
        let legacy: HostMembershipMarker = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.effective_grant, None);
    }

    #[test]
    fn custom_outfit_membership_recovers_for_generic_runtime_without_sidecar() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller = ControllerIdentity::from_secret([82_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let identity_store = DeviceIdentityStore::new(layout.clone());
        let pending = identity_store
            .prepare_pending(controller.public_identity(), site_id, invite_id)
            .unwrap();
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let pass = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Outfit Lab".into(),
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
        let mut profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        profile.outfit_id = "huang-lab".into();
        profile.display_name = "Huang Lab".into();
        profile.revision = 3;
        profile.validate().unwrap();
        let flavor = ClientFlavor::from_outfit_current(&profile).unwrap();
        let store = HostMembershipStore::new(layout.clone());
        let active = store
            .activate_with_network(
                flavor.id().unwrap(),
                Some(flavor.clone()),
                Some(profile.clone()),
                "Outfit Lab",
                &pending,
                &receipt,
                "GPU-02",
                None,
                None,
                None,
            )
            .unwrap();
        registry
            .finalize_host_persist(invite_id, receipt.device_id, receipt.persist_ack_token())
            .unwrap();

        assert_eq!(active.marker.outfit_profile.as_ref(), Some(&profile));
        let runtime = ClientFlavor::clew_original_current();
        let recovered = store.recover_for_runtime(&runtime).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].marker.client_flavor.as_ref(), Some(&flavor));
        assert_eq!(recovered[0].marker.outfit_profile.as_ref(), Some(&profile));

        let mut wrong_runtime = runtime;
        wrong_runtime.arch.push_str("-other");
        assert!(
            store
                .recover_for_runtime(&wrong_runtime)
                .unwrap()
                .is_empty()
        );
    }
}
