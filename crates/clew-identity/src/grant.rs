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

    pub const EXECUTE_READ_WRITE_CONNECTOR: Self = Self {
        member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
        read: true,
        write: true,
        shell: false,
    };

    pub const EXECUTE_READ_WRITE_SHELL_CONNECTOR: Self = Self {
        member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
        read: true,
        write: true,
        shell: true,
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

    #[test]
    fn full_execute_ceiling_never_adds_unsigned_write_or_shell() {
        let read_only = PermissionGrant::EXECUTE_READ_CONNECTOR
            .intersect(PermissionGrant::EXECUTE_READ_WRITE_SHELL_CONNECTOR);
        assert!(read_only.member.execute);
        assert!(read_only.member.connector);
        assert!(read_only.read);
        assert!(!read_only.write);
        assert!(!read_only.shell);

        let write_only = PermissionGrant::EXECUTE_READ_WRITE_CONNECTOR
            .intersect(PermissionGrant::EXECUTE_READ_WRITE_SHELL_CONNECTOR);
        assert!(write_only.read);
        assert!(write_only.write);
        assert!(!write_only.shell);

        let requested = PermissionGrant {
            member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
            read: true,
            write: true,
            shell: true,
        };
        let effective = requested.intersect(PermissionGrant::EXECUTE_READ_WRITE_SHELL_CONNECTOR);
        assert!(effective.read);
        assert!(effective.write);
        assert!(effective.shell);
    }
}
