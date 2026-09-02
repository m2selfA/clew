use clew_core::MemberCapabilities;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub member: MemberCapabilities,
    pub read: bool,
    pub write: bool,
    pub shell: bool,
}

impl PermissionGrant {
    pub const EXECUTE_READ: Self = Self {
        member: MemberCapabilities::EXECUTE_ONLY,
        read: true,
        write: false,
        shell: false,
    };

    pub const EXECUTE_READ_CONNECTOR: Self = Self {
        member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
        read: true,
        write: false,
        shell: false,
    };

    pub const CONNECTOR_ONLY: Self = Self {
        member: MemberCapabilities::CONNECTOR_ONLY,
        read: false,
        write: false,
        shell: false,
    };

    #[must_use]
    pub const fn intersect(self, ceiling: Self) -> Self {
        let member = MemberCapabilities {
            execute: self.member.execute && ceiling.member.execute,
            connector: self.member.connector && ceiling.member.connector,
        };
        let executable = member.execute;
        Self {
            member,
            read: executable && self.read && ceiling.read,
            write: executable && self.write && ceiling.write,
            shell: executable && self.shell && ceiling.shell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_only_intersection_cannot_gain_execution_tools() {
        let requested = PermissionGrant {
            member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
            read: true,
            write: true,
            shell: true,
        };
        let effective = requested.intersect(PermissionGrant::CONNECTOR_ONLY);
        assert!(!effective.member.execute);
        assert!(effective.member.connector);
        assert!(!effective.read);
        assert!(!effective.write);
        assert!(!effective.shell);
    }
}
