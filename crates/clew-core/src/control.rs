use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{DeviceId, DeviceNameOrigin, DeviceRecord, SiteId};

pub const HARD_MAX_READ_RESULT_BYTES: u32 = 48 * 1024;
pub const HARD_MAX_READ_TIMEOUT_MS: u32 = 30_000;
pub const HARD_MAX_READ_ROOTS: usize = 16;
pub const HARD_MAX_READ_ROOT_BYTES: usize = 2048;
pub const HARD_MAX_ACTIVITY_EVENTS: usize = 1024;
pub const HARD_ACTIVITY_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadPolicy {
    pub roots: Vec<String>,
    pub max_result_bytes: u32,
    pub timeout_ms: u32,
}

impl ReadPolicy {
    pub fn new(
        roots: Vec<String>,
        max_result_bytes: u32,
        timeout_ms: u32,
    ) -> Result<Self, ControlModelError> {
        let policy = Self {
            roots,
            max_result_bytes,
            timeout_ms,
        };
        policy.validate()?;
        Ok(policy)
    }

    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            roots: Vec::new(),
            max_result_bytes: HARD_MAX_READ_RESULT_BYTES,
            timeout_ms: 5_000,
        }
    }

    pub fn validate(&self) -> Result<(), ControlModelError> {
        if self.roots.len() > HARD_MAX_READ_ROOTS {
            return Err(ControlModelError::TooManyReadRoots(self.roots.len()));
        }
        for root in &self.roots {
            let trimmed = root.trim();
            if trimmed.is_empty() || trimmed.len() > HARD_MAX_READ_ROOT_BYTES {
                return Err(ControlModelError::InvalidReadRoot);
            }
        }
        if self.max_result_bytes == 0 || self.max_result_bytes > HARD_MAX_READ_RESULT_BYTES {
            return Err(ControlModelError::InvalidReadResultLimit(
                self.max_result_bytes,
            ));
        }
        if self.timeout_ms == 0 || self.timeout_ms > HARD_MAX_READ_TIMEOUT_MS {
            return Err(ControlModelError::InvalidReadTimeout(self.timeout_ms));
        }
        Ok(())
    }

    #[must_use]
    pub fn allows_read(&self) -> bool {
        !self.roots.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControllerSiteRecord {
    pub site_id: SiteId,
    pub site_name: String,
    pub read_policy: ReadPolicy,
    pub revoked: bool,
}

impl ControllerSiteRecord {
    pub fn validate(&self) -> Result<(), ControlModelError> {
        validate_name(&self.site_name)?;
        self.read_policy.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControllerDeviceRecord {
    pub device: DeviceRecord,
    pub revoked: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControllerCatalog {
    pub sites: BTreeMap<SiteId, ControllerSiteRecord>,
    pub devices: BTreeMap<DeviceId, ControllerDeviceRecord>,
}

impl ControllerCatalog {
    pub fn validate(&self) -> Result<(), ControlModelError> {
        for site in self.sites.values() {
            site.validate()?;
        }
        for record in self.devices.values() {
            if !self.sites.contains_key(&record.device.site_id) {
                return Err(ControlModelError::UnknownSite(record.device.site_id));
            }
        }
        Ok(())
    }

    pub fn upsert_site(&mut self, site: ControllerSiteRecord) -> Result<(), ControlModelError> {
        site.validate()?;
        if let Some(existing) = self.sites.get(&site.site_id)
            && existing.site_name != site.site_name
        {
            return Err(ControlModelError::SiteConflict(site.site_id));
        }
        self.sites.insert(site.site_id, site);
        Ok(())
    }

    pub fn register_device(&mut self, device: DeviceRecord) -> Result<(), ControlModelError> {
        if !self.sites.contains_key(&device.site_id) {
            return Err(ControlModelError::UnknownSite(device.site_id));
        }
        if let Some(existing) = self.devices.get(&device.device_id)
            && existing.device != device
        {
            return Err(ControlModelError::DeviceConflict(device.device_id));
        }
        self.devices.insert(
            device.device_id,
            ControllerDeviceRecord {
                device,
                revoked: false,
            },
        );
        Ok(())
    }

    pub fn rename_device(
        &mut self,
        device_id: DeviceId,
        display_name: &str,
    ) -> Result<DeviceRecord, ControlModelError> {
        let display_name = validate_name(display_name)?;
        let record = self
            .devices
            .get_mut(&device_id)
            .ok_or(ControlModelError::UnknownDevice(device_id))?;
        record.device.display_name = display_name;
        record.device.name_origin = DeviceNameOrigin::Renamed;
        Ok(record.device.clone())
    }

    pub fn revoke_device(&mut self, device_id: DeviceId) -> Result<(), ControlModelError> {
        let record = self
            .devices
            .get_mut(&device_id)
            .ok_or(ControlModelError::UnknownDevice(device_id))?;
        record.revoked = true;
        Ok(())
    }

    pub fn revoke_site(&mut self, site_id: SiteId) -> Result<Vec<DeviceId>, ControlModelError> {
        let site = self
            .sites
            .get_mut(&site_id)
            .ok_or(ControlModelError::UnknownSite(site_id))?;
        site.revoked = true;
        let mut revoked = Vec::new();
        for (device_id, record) in &mut self.devices {
            if record.device.site_id == site_id {
                record.revoked = true;
                revoked.push(*device_id);
            }
        }
        Ok(revoked)
    }

    #[must_use]
    pub fn site(&self, site_id: SiteId) -> Option<&ControllerSiteRecord> {
        self.sites.get(&site_id)
    }

    #[must_use]
    pub fn device(&self, device_id: DeviceId) -> Option<&ControllerDeviceRecord> {
        self.devices.get(&device_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityResult {
    Succeeded,
    Denied,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub sequence: u64,
    pub unix_ms: u64,
    pub site_id: SiteId,
    pub device_id: DeviceId,
    pub operation: String,
    pub path_summary: Option<String>,
    pub result: ActivityResult,
    pub duration_ms: u64,
    pub transferred_bytes: u64,
}

impl ActivityEvent {
    pub fn validate(&self) -> Result<(), ControlModelError> {
        if self.operation.is_empty() || self.operation.len() > 64 {
            return Err(ControlModelError::InvalidActivityOperation);
        }
        if self
            .path_summary
            .as_ref()
            .is_some_and(|value| value.len() > HARD_MAX_READ_ROOT_BYTES)
        {
            return Err(ControlModelError::InvalidActivityPath);
        }
        Ok(())
    }
}

fn validate_name(value: &str) -> Result<String, ControlModelError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(ControlModelError::InvalidName);
    }
    Ok(value.to_owned())
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ControlModelError {
    #[error("read policy has too many roots: {0}")]
    TooManyReadRoots(usize),
    #[error("read root must be 1..={HARD_MAX_READ_ROOT_BYTES} UTF-8 bytes")]
    InvalidReadRoot,
    #[error("read result limit must be 1..={HARD_MAX_READ_RESULT_BYTES} bytes, got {0}")]
    InvalidReadResultLimit(u32),
    #[error("read timeout must be 1..={HARD_MAX_READ_TIMEOUT_MS} ms, got {0}")]
    InvalidReadTimeout(u32),
    #[error("display/site name must be 1..=128 UTF-8 bytes")]
    InvalidName,
    #[error("unknown SiteId {0}")]
    UnknownSite(SiteId),
    #[error("unknown DeviceId {0}")]
    UnknownDevice(DeviceId),
    #[error("SiteId {0} conflicts with existing site metadata")]
    SiteConflict(SiteId),
    #[error("DeviceId {0} conflicts with existing device metadata")]
    DeviceConflict(DeviceId),
    #[error("activity operation must be 1..=64 bytes")]
    InvalidActivityOperation,
    #[error("activity path summary is too long")]
    InvalidActivityPath,
}

#[cfg(test)]
mod tests {
    use crate::{InviteId, MemberCapabilities};

    use super::*;

    fn site() -> ControllerSiteRecord {
        ControllerSiteRecord {
            site_id: SiteId::new(),
            site_name: "Alice Lab".into(),
            read_policy: ReadPolicy::new(vec!["D:/shared".into()], 4096, 2000).unwrap(),
            revoked: false,
        }
    }

    #[test]
    fn read_policy_enforces_built_in_bounds() {
        assert!(ReadPolicy::new(vec!["D:/shared".into()], 4096, 2000).is_ok());
        assert!(
            ReadPolicy::new(vec!["D:/shared".into()], HARD_MAX_READ_RESULT_BYTES + 1, 1).is_err()
        );
        assert!(ReadPolicy::new(vec!["x".repeat(HARD_MAX_READ_ROOT_BYTES + 1)], 1, 1).is_err());
    }

    #[test]
    fn catalog_rename_is_explicit_and_revoke_is_persistent_projection() {
        let mut catalog = ControllerCatalog::default();
        let site = site();
        let site_id = site.site_id;
        catalog.upsert_site(site).unwrap();
        let device_id = DeviceId::new();
        catalog
            .register_device(DeviceRecord {
                device_id,
                site_id,
                display_name: "GPU-01".into(),
                hostname_observed: "GPU-01".into(),
                capabilities: MemberCapabilities::EXECUTE_ONLY,
                enrolled_via_invite_id: InviteId::new(),
                name_origin: DeviceNameOrigin::Automatic {
                    base_hostname: "GPU-01".into(),
                    tagged: false,
                    tag_generation: 0,
                },
            })
            .unwrap();
        let renamed = catalog.rename_device(device_id, "Microscope PC").unwrap();
        assert_eq!(renamed.display_name, "Microscope PC");
        assert_eq!(renamed.name_origin, DeviceNameOrigin::Renamed);
        catalog.revoke_device(device_id).unwrap();
        assert!(catalog.device(device_id).unwrap().revoked);
    }
}
