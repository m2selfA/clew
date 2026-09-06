use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{ControllerId, DeviceId, SiteId};

pub const STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_STATE_DOCUMENT_SIZE: usize = 16 * 1024 * 1024;
const STATE_LAYOUT_VERSION_DIR: &str = "v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StateEnvelope<T> {
    pub schema_version: u32,
    pub payload: T,
}

impl<T> StateEnvelope<T> {
    #[must_use]
    pub const fn new(payload: T) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            payload,
        }
    }
}

pub fn encode_state_json<T: Serialize>(payload: &T) -> Result<Vec<u8>, StateCodecError> {
    let encoded = serde_json::to_vec_pretty(&StateEnvelope::new(payload))?;
    check_document_size(encoded.len())?;
    Ok(encoded)
}

pub fn decode_state_json<T: DeserializeOwned>(input: &[u8]) -> Result<T, StateCodecError> {
    check_document_size(input.len())?;
    #[derive(Deserialize)]
    struct Header {
        schema_version: u32,
    }

    let header: Header = serde_json::from_slice(input)?;
    if header.schema_version != STATE_SCHEMA_VERSION {
        return Err(StateCodecError::UnsupportedSchemaVersion {
            found: header.schema_version,
            supported: STATE_SCHEMA_VERSION,
        });
    }

    let envelope: StateEnvelope<T> = serde_json::from_slice(input)?;
    Ok(envelope.payload)
}

fn check_document_size(actual: usize) -> Result<(), StateCodecError> {
    if actual > MAX_STATE_DOCUMENT_SIZE {
        return Err(StateCodecError::DocumentTooLarge {
            actual,
            max: MAX_STATE_DOCUMENT_SIZE,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateLayout {
    root: PathBuf,
}

impl StateLayout {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn version_root(&self) -> PathBuf {
        self.root.join(STATE_LAYOUT_VERSION_DIR)
    }

    #[must_use]
    pub fn controller_state_path(&self) -> PathBuf {
        self.version_root().join("controller.json")
    }

    #[must_use]
    pub fn controller_lock_path(&self) -> PathBuf {
        self.version_root().join("controller.lock")
    }

    #[must_use]
    pub fn local_api_secret_path(&self) -> PathBuf {
        self.version_root().join("local-api.secret")
    }

    #[must_use]
    pub fn local_api_socket_path(&self) -> PathBuf {
        self.version_root().join("controller.sock")
    }

    #[must_use]
    pub fn controller_control_slot_a_path(&self) -> PathBuf {
        self.version_root().join("controller-control.a.json")
    }

    #[must_use]
    pub fn controller_control_slot_b_path(&self) -> PathBuf {
        self.version_root().join("controller-control.b.json")
    }

    #[must_use]
    pub fn controller_file_transfer_slot_a_path(&self) -> PathBuf {
        self.version_root().join("controller-file-transfers.a.json")
    }

    #[must_use]
    pub fn controller_file_transfer_slot_b_path(&self) -> PathBuf {
        self.version_root().join("controller-file-transfers.b.json")
    }

    #[must_use]
    pub fn controller_directory_transfer_slot_a_path(&self) -> PathBuf {
        self.version_root()
            .join("controller-directory-transfers.a.json")
    }

    #[must_use]
    pub fn controller_directory_transfer_slot_b_path(&self) -> PathBuf {
        self.version_root()
            .join("controller-directory-transfers.b.json")
    }

    #[must_use]
    pub fn outfit_library_slot_a_path(&self) -> PathBuf {
        self.version_root().join("outfit-library.a.json")
    }

    #[must_use]
    pub fn outfit_library_slot_b_path(&self) -> PathBuf {
        self.version_root().join("outfit-library.b.json")
    }

    #[must_use]
    pub fn outfit_assets_root(&self) -> PathBuf {
        self.version_root().join("outfit-assets")
    }

    #[must_use]
    pub fn client_flavor_artifacts_root(&self) -> PathBuf {
        self.version_root().join("client-flavors")
    }

    #[must_use]
    pub fn membership_dir(&self, controller_id: ControllerId, site_id: SiteId) -> PathBuf {
        self.version_root()
            .join("memberships")
            .join(controller_id.to_string())
            .join(site_id.to_string())
    }

    #[must_use]
    pub fn pending_device_identity_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("device-key.pending.json")
    }

    #[must_use]
    pub fn active_device_identity_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("device-key.json")
    }

    #[must_use]
    pub fn pending_controller_activation_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("controller-activation.pending.json")
    }

    #[must_use]
    pub fn host_membership_marker_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("membership.json")
    }

    #[must_use]
    pub fn nearby_connector_import_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("nearby-connector.import.json")
    }

    #[must_use]
    pub fn nearby_connector_export_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("nearby-connector.export.json")
    }

    #[must_use]
    pub fn host_file_transfers_root(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("file-transfers")
    }

    #[must_use]
    pub fn host_managed_fs_root(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("host")
            .join("managed-fs")
            .join(device_id.to_string())
    }

    #[must_use]
    pub fn device_record_path(
        &self,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
    ) -> PathBuf {
        self.membership_dir(controller_id, site_id)
            .join("devices")
            .join(format!("{device_id}.json"))
    }
}

#[derive(Debug, Error)]
pub enum StateCodecError {
    #[error("state document is {actual} bytes; maximum is {max}")]
    DocumentTooLarge { actual: usize, max: usize },
    #[error("state JSON is malformed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported state schema version {found}; this build supports {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{DeviceNameOrigin, DeviceRecord, InviteId, MemberCapabilities};

    fn sample_record() -> DeviceRecord {
        DeviceRecord {
            device_id: DeviceId::new(),
            site_id: SiteId::new(),
            display_name: "GPU-01".into(),
            hostname_observed: "GPU-01".into(),
            capabilities: MemberCapabilities::EXECUTE_ONLY,
            enrolled_via_invite_id: InviteId::new(),
            name_origin: DeviceNameOrigin::Automatic {
                base_hostname: "GPU-01".into(),
                tagged: false,
                tag_generation: 0,
            },
        }
    }

    #[test]
    fn versioned_state_roundtrips_device_record() {
        let record = sample_record();
        let encoded = encode_state_json(&record).unwrap();
        assert_eq!(decode_state_json::<DeviceRecord>(&encoded).unwrap(), record);
    }

    #[test]
    fn unsupported_version_is_reported_before_payload_shape_is_interpreted() {
        let encoded = br#"{"schema_version":2,"payload":{"future":"shape"}}"#;
        assert!(matches!(
            decode_state_json::<DeviceRecord>(encoded),
            Err(StateCodecError::UnsupportedSchemaVersion {
                found: 2,
                supported: STATE_SCHEMA_VERSION
            })
        ));
    }

    #[test]
    fn malformed_or_missing_version_state_fails_closed() {
        assert!(decode_state_json::<DeviceRecord>(br#"{"payload":{}}"#).is_err());
        assert!(decode_state_json::<DeviceRecord>(b"not json").is_err());
    }

    #[test]
    fn oversized_state_is_rejected_before_json_parsing() {
        let oversized = vec![b' '; MAX_STATE_DOCUMENT_SIZE + 1];
        assert!(matches!(
            decode_state_json::<DeviceRecord>(&oversized),
            Err(StateCodecError::DocumentTooLarge {
                actual,
                max: MAX_STATE_DOCUMENT_SIZE
            }) if actual == MAX_STATE_DOCUMENT_SIZE + 1
        ));
    }

    #[test]
    fn unknown_fields_are_tolerated_within_the_same_schema_version() {
        let record = sample_record();
        let mut value = serde_json::to_value(StateEnvelope::new(record.clone())).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_metadata".into(), json!({"ignored": true}));
        let encoded = serde_json::to_vec(&value).unwrap();
        assert_eq!(decode_state_json::<DeviceRecord>(&encoded).unwrap(), record);
    }

    #[test]
    fn layout_is_explicitly_versioned_and_scoped_by_controller_site_and_device() {
        let controller = ControllerId::new();
        let site = SiteId::new();
        let device = DeviceId::new();
        let layout = StateLayout::new("state-root");
        assert_eq!(
            layout.controller_state_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller.json")
        );
        assert_eq!(
            layout.controller_lock_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller.lock")
        );
        assert_eq!(
            layout.controller_directory_transfer_slot_a_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller-directory-transfers.a.json")
        );
        assert_eq!(
            layout.controller_directory_transfer_slot_b_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller-directory-transfers.b.json")
        );
        assert_eq!(
            layout.local_api_secret_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("local-api.secret")
        );
        assert_eq!(
            layout.local_api_socket_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller.sock")
        );
        assert_eq!(
            layout.controller_control_slot_a_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller-control.a.json")
        );
        assert_eq!(
            layout.controller_control_slot_b_path(),
            PathBuf::from("state-root")
                .join("v1")
                .join("controller-control.b.json")
        );
        assert_eq!(
            layout.pending_device_identity_path(controller, site),
            PathBuf::from("state-root")
                .join("v1")
                .join("memberships")
                .join(controller.to_string())
                .join(site.to_string())
                .join("host")
                .join("device-key.pending.json")
        );
        assert_eq!(
            layout.active_device_identity_path(controller, site),
            PathBuf::from("state-root")
                .join("v1")
                .join("memberships")
                .join(controller.to_string())
                .join(site.to_string())
                .join("host")
                .join("device-key.json")
        );
        assert_eq!(
            layout.pending_controller_activation_path(controller, site),
            PathBuf::from("state-root")
                .join("v1")
                .join("memberships")
                .join(controller.to_string())
                .join(site.to_string())
                .join("host")
                .join("controller-activation.pending.json")
        );
        assert_eq!(
            layout.device_record_path(controller, site, device),
            PathBuf::from("state-root")
                .join("v1")
                .join("memberships")
                .join(controller.to_string())
                .join(site.to_string())
                .join("devices")
                .join(format!("{device}.json"))
        );
    }
}
