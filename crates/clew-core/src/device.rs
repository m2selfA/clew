use serde::{Deserialize, Serialize};

use crate::{DeviceId, InviteId, SiteId};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemberCapabilities {
    pub execute: bool,
    pub connector: bool,
}

impl MemberCapabilities {
    pub const EXECUTE_ONLY: Self = Self {
        execute: true,
        connector: false,
    };
    pub const CONNECTOR_ONLY: Self = Self {
        execute: false,
        connector: true,
    };
    pub const EXECUTE_AND_CONNECTOR: Self = Self {
        execute: true,
        connector: true,
    };

    #[must_use]
    pub const fn is_executable(self) -> bool {
        self.execute
    }

    #[must_use]
    pub const fn is_connector(self) -> bool {
        self.connector
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SiteMember {
    pub device_id: DeviceId,
    pub site_id: SiteId,
    pub capabilities: MemberCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceNameOrigin {
    Automatic {
        base_hostname: String,
        tagged: bool,
        tag_generation: u32,
    },
    Renamed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub site_id: SiteId,
    pub display_name: String,
    pub hostname_observed: String,
    pub capabilities: MemberCapabilities,
    pub enrolled_via_invite_id: InviteId,
    pub name_origin: DeviceNameOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub device_id: DeviceId,
    pub site_id: SiteId,
    pub site_name: String,
    pub display_name: String,
    pub hostname_observed: String,
    pub online: bool,
    pub executable: bool,
    pub connector: bool,
    pub last_seen_unix_ms: Option<u64>,
}

impl DeviceSummary {
    #[must_use]
    pub fn from_record(record: &DeviceRecord, site_name: impl Into<String>, online: bool) -> Self {
        Self {
            device_id: record.device_id,
            site_id: record.site_id,
            site_name: site_name.into(),
            display_name: record.display_name.clone(),
            hostname_observed: record.hostname_observed.clone(),
            online,
            executable: record.capabilities.execute,
            connector: record.capabilities.connector,
            last_seen_unix_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_only_is_never_executable() {
        assert!(!MemberCapabilities::CONNECTOR_ONLY.is_executable());
        assert!(MemberCapabilities::CONNECTOR_ONLY.is_connector());
    }

    #[test]
    fn device_summary_projects_capabilities_explicitly() {
        let record = DeviceRecord {
            device_id: DeviceId::new(),
            site_id: SiteId::new(),
            display_name: "Lab-PC".into(),
            hostname_observed: "Lab-PC".into(),
            capabilities: MemberCapabilities::CONNECTOR_ONLY,
            enrolled_via_invite_id: InviteId::new(),
            name_origin: DeviceNameOrigin::Automatic {
                base_hostname: "Lab-PC".into(),
                tagged: false,
                tag_generation: 0,
            },
        };
        let summary = DeviceSummary::from_record(&record, "Alice Lab", true);
        assert!(!summary.executable);
        assert!(summary.connector);
    }
}
