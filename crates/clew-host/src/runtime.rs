use std::path::{Path, PathBuf};

use clew_core::{DeviceId, SiteId, StateLayout};
use clew_identity::{
    ControllerPublicIdentity, DeviceIdentityStore, DeviceIdentityStoreError, DevicePublicIdentity,
    PendingDeviceIdentity,
};
use thiserror::Error;

use crate::{
    ClientFlavor, HostInstanceKey, HostMembership, HostMembershipError, HostMembershipMarker,
    HostMembershipStore, OutfitRuntimeView, SignedSiteClew, SiteClewError, observed_hostname,
};

#[derive(Clone, Debug)]
pub struct HostLaunchContext {
    pub explicit_site: Option<PathBuf>,
    pub executable_path: PathBuf,
    pub state_layout: StateLayout,
    pub client_flavor: ClientFlavor,
    pub archive_temp_detected: bool,
}

impl HostLaunchContext {
    pub fn current(
        explicit_site: Option<PathBuf>,
        state_layout: StateLayout,
    ) -> Result<Self, HostLaunchError> {
        let executable_path = std::env::current_exe()?;
        Ok(Self {
            explicit_site,
            archive_temp_detected: detect_archive_temp_launch(&executable_path),
            executable_path,
            state_layout,
            client_flavor: ClientFlavor::clew_original_current(),
        })
    }
}

#[derive(Clone, Debug)]
pub enum HostLaunchState {
    Active {
        membership: HostMembership,
        source: HostSiteSource,
    },
    AwaitingEnrollment {
        site_file: SignedSiteClew,
        pending: PendingDeviceIdentity,
        hostname: String,
        source: HostSiteSource,
    },
    AmbiguousMembership {
        candidates: Vec<HostMembershipMarker>,
        client_flavor: ClientFlavor,
    },
    MissingInvite {
        view: MissingInviteView,
        client_flavor: ClientFlavor,
    },
}

impl HostLaunchState {
    pub fn instance_key(&self) -> Result<HostInstanceKey, HostLaunchError> {
        match self {
            Self::Active { membership, .. } => Ok(HostInstanceKey::membership(
                membership.marker.controller.controller_id,
                membership.marker.site_id,
            )),
            Self::AwaitingEnrollment { site_file, .. } => Ok(HostInstanceKey::membership(
                site_file.payload.bootstrap.payload.controller.controller_id,
                site_file.payload.bootstrap.payload.site_id,
            )),
            Self::AmbiguousMembership { client_flavor, .. }
            | Self::MissingInvite { client_flavor, .. } => {
                Ok(HostInstanceKey::missing_invite(client_flavor.id()?))
            }
        }
    }

    #[must_use]
    pub fn site_name(&self) -> Option<&str> {
        match self {
            Self::Active { membership, .. } => Some(&membership.marker.site_name),
            Self::AwaitingEnrollment { site_file, .. } => {
                Some(&site_file.payload.bootstrap.payload.site_name)
            }
            Self::AmbiguousMembership { .. } | Self::MissingInvite { .. } => None,
        }
    }

    #[must_use]
    pub fn device_id(&self) -> Option<DeviceId> {
        match self {
            Self::Active { membership, .. } => Some(membership.marker.device_id),
            _ => None,
        }
    }

    #[must_use]
    pub fn device_public_identity(&self) -> Option<DevicePublicIdentity> {
        match self {
            Self::Active { membership, .. } => Some(membership.identity.public_identity()),
            Self::AwaitingEnrollment { pending, .. } => Some(pending.public_identity()),
            _ => None,
        }
    }

    #[must_use]
    pub fn controller(&self) -> Option<ControllerPublicIdentity> {
        match self {
            Self::Active { membership, .. } => Some(membership.marker.controller),
            Self::AwaitingEnrollment { site_file, .. } => {
                Some(site_file.payload.bootstrap.payload.controller)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn site_id(&self) -> Option<SiteId> {
        match self {
            Self::Active { membership, .. } => Some(membership.marker.site_id),
            Self::AwaitingEnrollment { site_file, .. } => {
                Some(site_file.payload.bootstrap.payload.site_id)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSiteSource {
    Explicit,
    ExecutableSibling,
    LocalMembership,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingInviteView {
    pub title: String,
    pub body: String,
    pub extract_first: Option<String>,
    pub choose_button: String,
}

pub fn resolve_host_launch(context: HostLaunchContext) -> Result<HostLaunchState, HostLaunchError> {
    if let Some(explicit) = &context.explicit_site {
        let site_file = SignedSiteClew::read(explicit)?;
        return resolve_site_file(&context, site_file, HostSiteSource::Explicit);
    }

    let sibling = site_clew_sibling(&context.executable_path)?;
    if sibling.is_file() {
        let site_file = SignedSiteClew::read(&sibling)?;
        return resolve_site_file(&context, site_file, HostSiteSource::ExecutableSibling);
    }

    let flavor_id = context.client_flavor.id()?;
    let memberships =
        HostMembershipStore::new(context.state_layout.clone()).recover_for_flavor(flavor_id)?;
    match memberships.as_slice() {
        [only] => Ok(HostLaunchState::Active {
            membership: only.clone(),
            source: HostSiteSource::LocalMembership,
        }),
        [] => Ok(HostLaunchState::MissingInvite {
            view: missing_invite_view(context.archive_temp_detected),
            client_flavor: context.client_flavor,
        }),
        many => Ok(HostLaunchState::AmbiguousMembership {
            candidates: many.iter().map(|item| item.marker.clone()).collect(),
            client_flavor: context.client_flavor,
        }),
    }
}

fn resolve_site_file(
    context: &HostLaunchContext,
    site_file: SignedSiteClew,
    source: HostSiteSource,
) -> Result<HostLaunchState, HostLaunchError> {
    site_file.verify_for_flavor(&context.client_flavor)?;
    let controller = site_file.verify()?;
    let site_id = site_file.payload.bootstrap.payload.site_id;
    let site_name = site_file.payload.bootstrap.payload.site_name.clone();
    let flavor_id = context.client_flavor.id()?;
    let memberships = HostMembershipStore::new(context.state_layout.clone());
    if let Some(membership) =
        memberships.recover_active_from_site(flavor_id, controller, site_id, &site_name)?
    {
        return Ok(HostLaunchState::Active { membership, source });
    }

    let pending = DeviceIdentityStore::new(context.state_layout.clone()).prepare_pending(
        controller,
        site_id,
        site_file.payload.bootstrap.payload.invite_id,
    )?;
    Ok(HostLaunchState::AwaitingEnrollment {
        site_file,
        pending,
        hostname: observed_hostname(),
        source,
    })
}

fn site_clew_sibling(executable_path: &Path) -> Result<PathBuf, HostLaunchError> {
    #[cfg(target_os = "macos")]
    {
        for ancestor in executable_path.ancestors() {
            if ancestor.extension().and_then(|value| value.to_str()) == Some("app") {
                let kit_root = ancestor
                    .parent()
                    .ok_or(HostLaunchError::InvalidExecutablePath)?;
                return Ok(kit_root.join("site.clew"));
            }
        }
    }
    let parent = executable_path
        .parent()
        .ok_or(HostLaunchError::InvalidExecutablePath)?;
    Ok(parent.join("site.clew"))
}

#[must_use]
pub fn detect_archive_temp_launch(executable_path: &Path) -> bool {
    let _ = executable_path;
    #[cfg(windows)]
    {
        let temp = std::env::temp_dir();
        if executable_path.starts_with(&temp) {
            return executable_path.components().any(|component| {
                let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
                value.starts_with("temp") || value.contains(".zip")
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        if executable_path
            .to_string_lossy()
            .contains("/AppTranslocation/")
        {
            return true;
        }
    }
    false
}

fn missing_invite_view(archive_temp_detected: bool) -> MissingInviteView {
    let ui = OutfitRuntimeView::clew_original().resources;
    MissingInviteView {
        title: ui.missing_invite_title.into(),
        body: ui.missing_invite_body.into(),
        extract_first: archive_temp_detected.then(|| ui.extract_first.into()),
        choose_button: ui.choose_invite.into(),
    }
}

#[derive(Debug, Error)]
pub enum HostLaunchError {
    #[error(transparent)]
    Site(#[from] SiteClewError),
    #[error(transparent)]
    Membership(#[from] HostMembershipError),
    #[error(transparent)]
    IdentityStore(#[from] DeviceIdentityStoreError),
    #[error("host executable path has no parent")]
    InvalidExecutablePath,
    #[error("host launch I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use clew_core::{InviteId, MemberCapabilities};
    use clew_identity::{
        ControllerIdentity, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::{HostMembershipStore, HostRoleHint};

    fn site_file(
        controller: &ControllerIdentity,
        site_id: SiteId,
        invite_id: InviteId,
        site_name: &str,
        flavor: ClientFlavor,
    ) -> (SignedSiteClew, EnrollmentRegistry) {
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
                controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: site_name.into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 100,
                    deployment_window_ms: 50,
                    max_claims: 2,
                },
            )
            .unwrap();
        (
            SignedSiteClew::issue(controller, flavor, pass, HostRoleHint::ExecutePreferred)
                .unwrap(),
            registry,
        )
    }

    #[test]
    fn explicit_site_wins_over_sibling_and_reuses_pending_key() {
        let temp = tempdir().unwrap();
        let kit = temp.path().join("kit");
        std::fs::create_dir_all(&kit).unwrap();
        let state = StateLayout::new(temp.path().join("state"));
        let flavor = ClientFlavor::clew_original_current();
        let controller = ControllerIdentity::from_secret([91_u8; 32]);
        let explicit_site_id = SiteId::new();
        let explicit_invite = InviteId::new();
        let sibling_site_id = SiteId::new();
        let (explicit, _) = site_file(
            &controller,
            explicit_site_id,
            explicit_invite,
            "Explicit Lab",
            flavor.clone(),
        );
        let (sibling, _) = site_file(
            &controller,
            sibling_site_id,
            InviteId::new(),
            "Sibling Lab",
            flavor.clone(),
        );
        explicit.write(&kit.join("chosen.clew")).unwrap();
        sibling.write(&kit.join("site.clew")).unwrap();
        let executable = kit.join(if cfg!(windows) { "Clew.exe" } else { "Clew" });

        let context = HostLaunchContext {
            explicit_site: Some(kit.join("chosen.clew")),
            executable_path: executable.clone(),
            state_layout: state.clone(),
            client_flavor: flavor.clone(),
            archive_temp_detected: false,
        };
        let first = resolve_host_launch(context.clone()).unwrap();
        let first_public = first.device_public_identity().unwrap();
        assert_eq!(first.site_id(), Some(explicit_site_id));
        let second = resolve_host_launch(context).unwrap();
        assert_eq!(second.device_public_identity(), Some(first_public));
        assert_eq!(second.site_id(), Some(explicit_site_id));
    }

    #[test]
    fn missing_sidecar_recovers_unique_active_membership() {
        let temp = tempdir().unwrap();
        let state = StateLayout::new(temp.path().join("state"));
        let flavor = ClientFlavor::clew_original_current();
        let controller = ControllerIdentity::from_secret([92_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let (site, mut registry) =
            site_file(&controller, site_id, invite_id, "Alice Lab", flavor.clone());
        let pending = DeviceIdentityStore::new(state.clone())
            .prepare_pending(controller.public_identity(), site_id, invite_id)
            .unwrap();
        let receipt = registry
            .claim(&site.payload.bootstrap, pending.public_identity(), 10)
            .unwrap();
        HostMembershipStore::new(state.clone())
            .activate(
                flavor.id().unwrap(),
                "Alice Lab",
                &pending,
                &receipt,
                "GPU-01",
            )
            .unwrap();

        let result = resolve_host_launch(HostLaunchContext {
            explicit_site: None,
            executable_path: temp.path().join("no-kit").join("Clew.exe"),
            state_layout: state,
            client_flavor: flavor,
            archive_temp_detected: false,
        })
        .unwrap();
        assert!(matches!(
            result,
            HostLaunchState::Active {
                source: HostSiteSource::LocalMembership,
                ..
            }
        ));
        assert_eq!(result.device_id(), Some(receipt.device_id));
    }

    #[test]
    fn missing_invite_copy_does_not_scan_or_guess_cwd() {
        let temp = tempdir().unwrap();
        let result = resolve_host_launch(HostLaunchContext {
            explicit_site: None,
            executable_path: temp.path().join("kit").join("Clew.exe"),
            state_layout: StateLayout::new(temp.path().join("state")),
            client_flavor: ClientFlavor::clew_original_current(),
            archive_temp_detected: true,
        })
        .unwrap();
        let HostLaunchState::MissingInvite { view, .. } = result else {
            panic!("expected missing invite")
        };
        assert!(view.body.contains("site.clew"));
        assert!(view.extract_first.unwrap().contains("全部解压"));
    }
}
