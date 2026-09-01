use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use clew_core::{
    ActivityEvent, ActivityResult, ControlModelError, ControllerCatalog, DeviceId,
    HARD_ACTIVITY_RETENTION_MS, HARD_MAX_ACTIVITY_EVENTS, MAX_STATE_DOCUMENT_SIZE,
    MemberCapabilities, SiteId, StateCodecError, StateLayout, decode_state_json, encode_state_json,
};
use clew_identity::{
    EnrollmentError, EnrollmentRegistry, EnrollmentStatus, PermissionGrant, RecoveryReview,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControllerControlSnapshot {
    pub generation: u64,
    pub registry: EnrollmentRegistry,
    pub catalog: ControllerCatalog,
    pub activity: Vec<ActivityEvent>,
    pub next_activity_sequence: u64,
    pub recovery_review: Option<RecoveryReview>,
}

impl ControllerControlSnapshot {
    fn initial(controller_id: clew_core::ControllerId) -> Self {
        Self {
            generation: 0,
            registry: EnrollmentRegistry::new(
                controller_id,
                PermissionGrant {
                    member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
                    read: true,
                    write: true,
                    shell: true,
                },
            ),
            catalog: ControllerCatalog::default(),
            activity: Vec::new(),
            next_activity_sequence: 1,
            recovery_review: None,
        }
    }

    fn validate(&self, controller_id: clew_core::ControllerId) -> Result<(), ControlStoreError> {
        if self.registry.controller_id() != controller_id {
            return Err(ControlStoreError::ControllerMismatch);
        }
        self.catalog.validate()?;
        if self.activity.len() > HARD_MAX_ACTIVITY_EVENTS {
            return Err(ControlStoreError::TooManyActivityEvents(
                self.activity.len(),
            ));
        }
        let mut max_sequence = 0_u64;
        for event in &self.activity {
            event.validate()?;
            max_sequence = max_sequence.max(event.sequence);
        }
        if self.next_activity_sequence <= max_sequence {
            return Err(ControlStoreError::InvalidActivitySequence);
        }
        if self
            .recovery_review
            .is_some_and(|review| review.restored_controller_id != controller_id)
        {
            return Err(ControlStoreError::ControllerMismatch);
        }
        for (device_id, catalog_record) in &self.catalog.devices {
            if device_id != &catalog_record.device.device_id {
                return Err(ControlStoreError::CatalogRegistryMismatch(*device_id));
            }
            let enrollment = self
                .registry
                .device(*device_id)
                .ok_or(ControlStoreError::CatalogRegistryMismatch(*device_id))?;
            if enrollment.site_id != catalog_record.device.site_id
                || enrollment.invite_id != catalog_record.device.enrolled_via_invite_id
                || enrollment.effective_grant.member != catalog_record.device.capabilities
            {
                return Err(ControlStoreError::CatalogRegistryMismatch(*device_id));
            }
            let expected_status = if catalog_record.revoked {
                EnrollmentStatus::Revoked
            } else {
                EnrollmentStatus::Active
            };
            if enrollment.status != expected_status {
                return Err(ControlStoreError::CatalogRegistryMismatch(*device_id));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ControllerControlStore {
    layout: StateLayout,
    controller_id: clew_core::ControllerId,
    snapshot: ControllerControlSnapshot,
}

impl ControllerControlStore {
    pub fn load_or_create(
        layout: StateLayout,
        controller_id: clew_core::ControllerId,
    ) -> Result<Self, ControlStoreError> {
        let mut valid = Vec::new();
        let mut first_invalid = None;
        let mut any_present = false;
        for slot in [
            read_slot(&layout.controller_control_slot_a_path()),
            read_slot(&layout.controller_control_slot_b_path()),
        ] {
            match slot {
                SlotRead::Missing => {}
                SlotRead::Valid(snapshot) => {
                    any_present = true;
                    match snapshot.validate(controller_id) {
                        Ok(()) => valid.push(snapshot),
                        Err(error) => {
                            first_invalid.get_or_insert(error);
                        }
                    };
                }
                SlotRead::Invalid(error) => {
                    any_present = true;
                    first_invalid.get_or_insert(error);
                }
            }
        }

        if !valid.is_empty() {
            valid.sort_by_key(|snapshot| snapshot.generation);
            if valid.len() == 2 && valid[0].generation == valid[1].generation {
                return Err(ControlStoreError::GenerationConflict(valid[0].generation));
            }
            let snapshot = valid.pop().expect("valid snapshot exists");
            return Ok(Self {
                layout,
                controller_id,
                snapshot,
            });
        }

        if any_present {
            return Err(first_invalid.unwrap_or(ControlStoreError::NoValidSlot));
        }

        let mut store = Self {
            layout,
            controller_id,
            snapshot: ControllerControlSnapshot::initial(controller_id),
        };
        store.commit_candidate(store.snapshot.clone())?;
        Ok(store)
    }

    #[must_use]
    pub fn snapshot(&self) -> &ControllerControlSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn recovery_review(&self) -> Option<RecoveryReview> {
        self.snapshot.recovery_review
    }

    pub fn confirm_recovery_review(&mut self) -> Result<Option<RecoveryReview>, ControlStoreError> {
        self.transaction(|snapshot| {
            let Some(mut review) = snapshot.recovery_review else {
                return Ok(None);
            };
            review.remote_access_paused = false;
            snapshot.recovery_review = Some(review);
            Ok(Some(review))
        })
    }

    pub fn transaction<R, F>(&mut self, mutate: F) -> Result<R, ControlStoreError>
    where
        F: FnOnce(&mut ControllerControlSnapshot) -> Result<R, ControlStoreError>,
    {
        let mut candidate = self.snapshot.clone();
        let result = mutate(&mut candidate)?;
        candidate.validate(self.controller_id)?;
        self.commit_candidate(candidate)?;
        Ok(result)
    }

    pub fn record_activity(
        &mut self,
        unix_ms: u64,
        site_id: SiteId,
        device_id: DeviceId,
        operation: impl Into<String>,
        path_summary: Option<String>,
        result: ActivityResult,
        duration_ms: u64,
        transferred_bytes: u64,
    ) -> Result<ActivityEvent, ControlStoreError> {
        let operation = operation.into();
        self.transaction(move |snapshot| {
            let sequence = snapshot.next_activity_sequence;
            snapshot.next_activity_sequence = sequence
                .checked_add(1)
                .ok_or(ControlStoreError::ActivitySequenceOverflow)?;
            let event = ActivityEvent {
                sequence,
                unix_ms,
                site_id,
                device_id,
                operation,
                path_summary,
                result,
                duration_ms,
                transferred_bytes,
            };
            event.validate()?;
            let oldest = unix_ms.saturating_sub(HARD_ACTIVITY_RETENTION_MS);
            snapshot
                .activity
                .retain(|existing| existing.unix_ms >= oldest);
            snapshot.activity.push(event.clone());
            if snapshot.activity.len() > HARD_MAX_ACTIVITY_EVENTS {
                let excess = snapshot.activity.len() - HARD_MAX_ACTIVITY_EVENTS;
                snapshot.activity.drain(..excess);
            }
            Ok(event)
        })
    }

    pub fn clear_activity(&mut self) -> Result<(), ControlStoreError> {
        self.transaction(|snapshot| {
            snapshot.activity.clear();
            Ok(())
        })
    }

    pub fn replace_restored_state(
        &mut self,
        registry: EnrollmentRegistry,
        catalog: ControllerCatalog,
        recovery_review: RecoveryReview,
    ) -> Result<(), ControlStoreError> {
        self.transaction(move |snapshot| {
            snapshot.registry = registry;
            snapshot.catalog = catalog;
            snapshot.activity.clear();
            snapshot.recovery_review = Some(recovery_review);
            Ok(())
        })
    }

    fn commit_candidate(
        &mut self,
        mut candidate: ControllerControlSnapshot,
    ) -> Result<(), ControlStoreError> {
        candidate.generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or(ControlStoreError::GenerationOverflow)?;
        candidate.validate(self.controller_id)?;
        let path = if candidate.generation % 2 == 1 {
            self.layout.controller_control_slot_a_path()
        } else {
            self.layout.controller_control_slot_b_path()
        };
        write_slot(&path, &candidate)?;
        self.snapshot = candidate;
        Ok(())
    }
}

#[derive(Debug)]
enum SlotRead {
    Missing,
    Valid(ControllerControlSnapshot),
    Invalid(ControlStoreError),
}

fn read_slot(path: &Path) -> SlotRead {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SlotRead::Missing,
        Err(error) => return SlotRead::Invalid(error.into()),
    };
    if metadata.len() > MAX_STATE_DOCUMENT_SIZE as u64 {
        return SlotRead::Invalid(ControlStoreError::DocumentTooLarge(metadata.len()));
    }
    match fs::read(path)
        .map_err(ControlStoreError::from)
        .and_then(|bytes| decode_state_json(&bytes).map_err(ControlStoreError::from))
    {
        Ok(snapshot) => SlotRead::Valid(snapshot),
        Err(error) => SlotRead::Invalid(error),
    }
}

fn write_slot(path: &Path, snapshot: &ControllerControlSnapshot) -> Result<(), ControlStoreError> {
    let parent = path.parent().ok_or(ControlStoreError::InvalidStatePath)?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let encoded = encode_state_json(snapshot)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&encoded)?;
    file.sync_all()?;
    sync_parent(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum ControlStoreError {
    #[error("controller control state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] StateCodecError),
    #[error(transparent)]
    Model(#[from] ControlModelError),
    #[error(transparent)]
    Enrollment(#[from] EnrollmentError),
    #[error("controller control state belongs to a different Controller")]
    ControllerMismatch,
    #[error("controller control state has no valid slot")]
    NoValidSlot,
    #[error("controller control state document is too large: {0} bytes")]
    DocumentTooLarge(u64),
    #[error("controller control state path has no parent")]
    InvalidStatePath,
    #[error("controller control slots have conflicting generation {0}")]
    GenerationConflict(u64),
    #[error("controller control generation overflow")]
    GenerationOverflow,
    #[error("controller activity sequence overflow")]
    ActivitySequenceOverflow,
    #[error("controller activity sequence is inconsistent")]
    InvalidActivitySequence,
    #[error("controller activity contains too many events: {0}")]
    TooManyActivityEvents(usize),
    #[error("Controller catalog and enrollment registry disagree for DeviceId {0}")]
    CatalogRegistryMismatch(DeviceId),
}

#[cfg(test)]
mod tests {
    use clew_core::{ControllerSiteRecord, DeviceNameOrigin, DeviceRecord, InviteId, ReadPolicy};
    use clew_identity::{ControllerIdentity, DeviceIdentity, SiteBootstrapSpec};
    use tempfile::tempdir;

    use super::*;

    fn active_device(
        snapshot: &mut ControllerControlSnapshot,
        controller: &ControllerIdentity,
    ) -> DeviceId {
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let pass = snapshot
            .registry
            .issue_bootstrap(
                controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "State Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 1,
                },
            )
            .unwrap();
        snapshot
            .catalog
            .upsert_site(ControllerSiteRecord {
                site_id,
                site_name: "State Lab".into(),
                read_policy: ReadPolicy::new(vec!["D:/shared".into()], 4096, 2_000).unwrap(),
                revoked: false,
            })
            .unwrap();
        let device_identity = DeviceIdentity::from_secret([91_u8; 32]);
        let receipt = snapshot
            .registry
            .claim(&pass, device_identity.public_identity(), 100)
            .unwrap();
        snapshot
            .registry
            .finalize_host_persist(invite_id, receipt.device_id, receipt.persist_ack_token())
            .unwrap();
        snapshot
            .catalog
            .register_device(DeviceRecord {
                device_id: receipt.device_id,
                site_id,
                display_name: "GPU-01".into(),
                hostname_observed: "GPU-01".into(),
                capabilities: receipt.effective_grant.member,
                enrolled_via_invite_id: invite_id,
                name_origin: DeviceNameOrigin::Automatic {
                    base_hostname: "GPU-01".into(),
                    tagged: false,
                    tag_generation: 0,
                },
            })
            .unwrap();
        receipt.device_id
    }

    #[test]
    fn recovery_review_is_persisted_and_requires_explicit_confirmation() {
        let temp = tempdir().unwrap();
        let controller = ControllerIdentity::from_secret([75_u8; 32]);
        let layout = StateLayout::new(temp.path());
        let mut store =
            ControllerControlStore::load_or_create(layout.clone(), controller.controller_id())
                .unwrap();
        let review = RecoveryReview {
            restored_controller_id: controller.controller_id(),
            remote_access_paused: true,
            historical_bootstrap_closed: true,
        };
        store
            .replace_restored_state(
                store.snapshot().registry.clone(),
                store.snapshot().catalog.clone(),
                review,
            )
            .unwrap();
        assert!(store.recovery_review().unwrap().remote_access_paused);
        let confirmed = store.confirm_recovery_review().unwrap().unwrap();
        assert!(!confirmed.remote_access_paused);
        drop(store);
        let reloaded =
            ControllerControlStore::load_or_create(layout, controller.controller_id()).unwrap();
        assert!(!reloaded.recovery_review().unwrap().remote_access_paused);
        assert!(
            reloaded
                .recovery_review()
                .unwrap()
                .historical_bootstrap_closed
        );
    }

    #[test]
    fn dual_slot_recovers_previous_valid_generation_after_newest_corruption() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller = ControllerIdentity::from_secret([81_u8; 32]);
        let mut store =
            ControllerControlStore::load_or_create(layout.clone(), controller.controller_id())
                .unwrap();
        assert_eq!(store.snapshot().generation, 1);
        store
            .transaction(|snapshot| {
                let _ = active_device(snapshot, &controller);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.snapshot().generation, 2);
        fs::write(
            layout.controller_control_slot_b_path(),
            b"corrupted newest slot",
        )
        .unwrap();

        let recovered =
            ControllerControlStore::load_or_create(layout, controller.controller_id()).unwrap();
        assert_eq!(recovered.snapshot().generation, 1);
        assert_eq!(recovered.snapshot().registry.device_count(), 0);
    }

    #[test]
    fn existing_but_unreadable_control_state_never_silently_resets() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller = ControllerIdentity::from_secret([82_u8; 32]);
        fs::create_dir_all(layout.version_root()).unwrap();
        fs::write(layout.controller_control_slot_a_path(), b"broken").unwrap();
        assert!(
            ControllerControlStore::load_or_create(layout, controller.controller_id()).is_err()
        );
    }

    #[test]
    fn activity_is_bounded_by_age_count_and_clearable() {
        let temp = tempdir().unwrap();
        let controller = ControllerIdentity::from_secret([83_u8; 32]);
        let mut store = ControllerControlStore::load_or_create(
            StateLayout::new(temp.path()),
            controller.controller_id(),
        )
        .unwrap();
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        store
            .record_activity(
                1,
                site_id,
                device_id,
                "read",
                Some("D:/old.txt".into()),
                ActivityResult::Succeeded,
                1,
                1,
            )
            .unwrap();
        store
            .record_activity(
                HARD_ACTIVITY_RETENTION_MS + 2,
                site_id,
                device_id,
                "read",
                Some("D:/new.txt".into()),
                ActivityResult::Succeeded,
                1,
                1,
            )
            .unwrap();
        assert_eq!(store.snapshot().activity.len(), 1);
        assert_eq!(
            store.snapshot().activity[0].path_summary.as_deref(),
            Some("D:/new.txt")
        );
        store.clear_activity().unwrap();
        assert!(store.snapshot().activity.is_empty());
    }
}
