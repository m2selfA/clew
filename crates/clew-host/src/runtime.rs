use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use clew_core::{DeviceId, SiteId, StateLayout};
use clew_identity::{
    ControllerPublicIdentity, DeviceIdentityStore, DeviceIdentityStoreError, DevicePublicIdentity,
    PendingDeviceIdentity,
};
use thiserror::Error;

use crate::{
    ClientFlavor, HostInstanceKey, HostMembership, HostMembershipError, HostMembershipMarker,
    HostMembershipStore, LEGACY_NEARBY_CONNECTOR_FILE_NAME, MAX_OUTFIT_ASSET_BYTES,
    NEARBY_CONNECTOR_FILE_NAME, NearbyConnectorStore, OutfitError, OutfitProfile,
    OutfitRuntimeView, SignedSiteClew, SiteClewError, observed_hostname, verify_outfit_asset_bytes,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostLaunchMode {
    #[default]
    Default,
    ConnectorOnly,
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
        launch_mode: HostLaunchMode,
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
    pub fn outfit_runtime_view(&self) -> OutfitRuntimeView {
        let profile = match self {
            Self::Active { membership, .. } => membership.marker.outfit_profile.as_ref(),
            Self::AwaitingEnrollment { site_file, .. } => site_file.payload.outfit_profile.as_ref(),
            Self::AmbiguousMembership { .. } | Self::MissingInvite { .. } => None,
        };
        match profile {
            Some(profile) => {
                OutfitRuntimeView::from_profile(profile, &profile.strings.locale_default)
            }
            None => OutfitRuntimeView::clew_original(),
        }
    }

    #[must_use]
    pub fn is_connector_only(&self) -> bool {
        match self {
            Self::Active { membership, .. } => {
                membership.device.capabilities.connector && !membership.device.capabilities.execute
            }
            Self::AwaitingEnrollment {
                site_file,
                launch_mode,
                ..
            } => {
                *launch_mode == HostLaunchMode::ConnectorOnly
                    || site_file.payload.role_hint == crate::HostRoleHint::ConnectorOnly
            }
            Self::AmbiguousMembership { .. } | Self::MissingInvite { .. } => false,
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
    resolve_host_launch_with_mode(context, HostLaunchMode::Default)
}

pub fn resolve_host_launch_with_mode(
    context: HostLaunchContext,
    launch_mode: HostLaunchMode,
) -> Result<HostLaunchState, HostLaunchError> {
    if let Some(explicit) = &context.explicit_site {
        let site_file = SignedSiteClew::read(explicit)?;
        return resolve_site_file(
            &context,
            site_file,
            HostSiteSource::Explicit,
            explicit,
            launch_mode,
        );
    }

    let sibling = site_clew_sibling(&context.executable_path)?;
    if sibling.is_file() {
        let site_file = SignedSiteClew::read(&sibling)?;
        return resolve_site_file(
            &context,
            site_file,
            HostSiteSource::ExecutableSibling,
            &sibling,
            launch_mode,
        );
    }

    let memberships = HostMembershipStore::new(context.state_layout.clone())
        .recover_for_runtime(&context.client_flavor)?;
    match memberships.as_slice() {
        [only] => {
            import_nearby_connector_sibling(
                &context.state_layout,
                &sibling,
                only.marker.controller,
                only.marker.site_id,
            );
            Ok(HostLaunchState::Active {
                membership: only.clone(),
                source: HostSiteSource::LocalMembership,
            })
        }
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
    site_path: &Path,
    launch_mode: HostLaunchMode,
) -> Result<HostLaunchState, HostLaunchError> {
    let effective_flavor = site_file.effective_flavor_for_runtime(&context.client_flavor)?;
    if let Some(profile) = &site_file.payload.outfit_profile {
        sync_outfit_assets_from_site(&context.state_layout, site_path, profile)?;
    }
    let controller = site_file.verify()?;
    let site_id = site_file.payload.bootstrap.payload.site_id;
    import_nearby_connector_sibling(&context.state_layout, site_path, controller, site_id);
    let site_name = site_file.payload.bootstrap.payload.site_name.clone();
    let memberships = HostMembershipStore::new(context.state_layout.clone());
    if let Some(membership) = memberships.recover_active_from_site(
        effective_flavor,
        site_file.payload.outfit_profile.clone(),
        controller,
        site_id,
        &site_name,
        site_file.payload.controller_endpoint.clone(),
        site_file.payload.read_policy.clone(),
        site_file.payload.controller_bootstrap_noise_public_key,
    )? {
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
        launch_mode,
        source,
    })
}

pub fn cached_outfit_asset_path(
    layout: &StateLayout,
    asset_id: &str,
) -> Result<PathBuf, HostLaunchError> {
    let root = layout.outfit_assets_root();
    let mut found = None;
    for extension in ["png", "svg"] {
        let candidate = root.join(format!("{asset_id}.{extension}"));
        if candidate.is_file() {
            if found.is_some() {
                return Err(HostLaunchError::AmbiguousOutfitAsset(asset_id.into()));
            }
            found = Some(candidate);
        }
    }
    found.ok_or_else(|| HostLaunchError::MissingOutfitAsset(asset_id.into()))
}

fn sync_outfit_assets_from_site(
    layout: &StateLayout,
    site_path: &Path,
    profile: &OutfitProfile,
) -> Result<(), HostLaunchError> {
    let asset_ids = profile.imported_asset_ids();
    if asset_ids.is_empty() {
        return Ok(());
    }
    let kit_root = site_path
        .parent()
        .ok_or(HostLaunchError::InvalidExecutablePath)?;
    let source_root = kit_root.join("outfit-assets");
    let target_root = layout.outfit_assets_root();
    fs::create_dir_all(&target_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target_root, fs::Permissions::from_mode(0o700))?;
    }
    for asset_id in asset_ids {
        sync_one_outfit_asset(&source_root, &target_root, &asset_id)?;
    }
    Ok(())
}

fn sync_one_outfit_asset(
    source_root: &Path,
    target_root: &Path,
    asset_id: &str,
) -> Result<(), HostLaunchError> {
    let mut source = None;
    for extension in ["png", "svg"] {
        let candidate = source_root.join(format!("{asset_id}.{extension}"));
        if candidate.is_file() {
            if source.is_some() {
                return Err(HostLaunchError::AmbiguousOutfitAsset(asset_id.into()));
            }
            source = Some((extension, candidate));
        }
    }
    let (extension, source) =
        source.ok_or_else(|| HostLaunchError::MissingOutfitAsset(asset_id.into()))?;
    let bytes = read_bounded_outfit_asset(&source)?;
    verify_outfit_asset_bytes(asset_id, &bytes)?;

    let target = target_root.join(format!("{asset_id}.{extension}"));
    if target.exists() {
        let existing = read_bounded_outfit_asset(&target)?;
        verify_outfit_asset_bytes(asset_id, &existing)?;
        return Ok(());
    }

    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| {
        std::io::Error::other(format!("secure random generation failed: {error}"))
    })?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temp = target_root.join(format!(".asset-{}-{suffix}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    match fs::rename(&temp, &target) {
        Ok(()) => {}
        Err(error) if target.exists() => {
            let _ = fs::remove_file(&temp);
            let existing = read_bounded_outfit_asset(&target)?;
            verify_outfit_asset_bytes(asset_id, &existing)?;
            let _ = error;
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_bounded_outfit_asset(path: &Path) -> Result<Vec<u8>, HostLaunchError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_OUTFIT_ASSET_BYTES as u64
    {
        return Err(HostLaunchError::InvalidOutfitAsset(
            path.display().to_string(),
        ));
    }
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_OUTFIT_ASSET_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_OUTFIT_ASSET_BYTES {
        return Err(HostLaunchError::InvalidOutfitAsset(
            path.display().to_string(),
        ));
    }
    Ok(bytes)
}

fn import_nearby_connector_sibling(
    layout: &StateLayout,
    anchor: &Path,
    controller: ControllerPublicIdentity,
    site_id: SiteId,
) {
    let Some(parent) = anchor.parent() else {
        return;
    };
    let store = NearbyConnectorStore::new(layout.clone());
    for name in [
        NEARBY_CONNECTOR_FILE_NAME,
        LEGACY_NEARBY_CONNECTOR_FILE_NAME,
    ] {
        let candidate = parent.join(name);
        if candidate.is_file() && store.import_path(&candidate, &controller, site_id).is_ok() {
            return;
        }
    }
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
    #[error(transparent)]
    Outfit(#[from] OutfitError),
    #[error("required Outfit asset is missing: {0}")]
    MissingOutfitAsset(String),
    #[error("multiple stored Outfit asset formats exist for {0}")]
    AmbiguousOutfitAsset(String),
    #[error("Outfit asset is invalid or exceeds its hard bound: {0}")]
    InvalidOutfitAsset(String),
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
    use clew_transport::{NearbyConnectorFile, SignedConnectorLease, noise_static_public};
    use iroh::{EndpointAddr, SecretKey, TransportAddr};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        HostMembershipStore, HostRoleHint, OutfitAssetRef, OutfitPreset, OutfitProfile,
        outfit_asset_id_for_bytes,
    };

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
    fn connector_only_launch_mode_is_explicit_and_pending_only() {
        let temp = tempdir().unwrap();
        let kit = temp.path().join("kit");
        std::fs::create_dir_all(&kit).unwrap();
        let state = StateLayout::new(temp.path().join("state"));
        let flavor = ClientFlavor::clew_original_current();
        let controller = ControllerIdentity::from_secret([90_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let (site, _) = site_file(
            &controller,
            site_id,
            invite_id,
            "Helper Lab",
            flavor.clone(),
        );
        let sidecar = kit.join("site.clew");
        site.write(&sidecar).unwrap();
        let context = HostLaunchContext {
            explicit_site: Some(sidecar),
            executable_path: kit.join("Clew.exe"),
            state_layout: state,
            client_flavor: flavor,
            archive_temp_detected: false,
        };

        let helper =
            resolve_host_launch_with_mode(context.clone(), HostLaunchMode::ConnectorOnly).unwrap();
        assert!(helper.is_connector_only());
        assert!(matches!(
            helper,
            HostLaunchState::AwaitingEnrollment {
                launch_mode: HostLaunchMode::ConnectorOnly,
                ..
            }
        ));

        let normal = resolve_host_launch(context).unwrap();
        assert!(!normal.is_connector_only());
        assert!(matches!(
            normal,
            HostLaunchState::AwaitingEnrollment {
                launch_mode: HostLaunchMode::Default,
                ..
            }
        ));
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
    fn signed_outfit_assets_are_hash_verified_and_cached_before_enrollment() {
        let temp = tempdir().unwrap();
        let kit = temp.path().join("kit");
        let asset_dir = kit.join("outfit-assets");
        std::fs::create_dir_all(&asset_dir).unwrap();
        let state = StateLayout::new(temp.path().join("state"));
        let controller = ControllerIdentity::from_secret([93_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="12"><rect width="16" height="12"/></svg>"#;
        let asset_id = outfit_asset_id_for_bytes(bytes);
        let mut profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        profile.outfit_id = "asset-lab".into();
        profile.display_name = "Asset Lab".into();
        profile.revision = 2;
        profile.visuals.logo = Some(OutfitAssetRef::Imported {
            asset_id: asset_id.clone(),
        });
        profile.validate().unwrap();
        let flavor = ClientFlavor::from_outfit_current(&profile).unwrap();
        let (mut site, _) = site_file(&controller, site_id, invite_id, "Asset Lab", flavor.clone());
        site.payload.outfit_profile = Some(profile);
        site.signature = controller.sign_site_config(&site.payload).unwrap();
        let site_path = kit.join("site.clew");
        site.write(&site_path).unwrap();
        std::fs::write(asset_dir.join(format!("{asset_id}.svg")), bytes).unwrap();

        let result = resolve_host_launch(HostLaunchContext {
            explicit_site: Some(site_path.clone()),
            executable_path: kit.join("Clew.exe"),
            state_layout: state.clone(),
            client_flavor: ClientFlavor::clew_original_current(),
            archive_temp_detected: false,
        })
        .unwrap();
        assert!(matches!(result, HostLaunchState::AwaitingEnrollment { .. }));
        let cached = cached_outfit_asset_path(&state, &asset_id).unwrap();
        assert_eq!(std::fs::read(cached).unwrap(), bytes);

        std::fs::remove_dir_all(state.outfit_assets_root()).unwrap();
        std::fs::write(asset_dir.join(format!("{asset_id}.svg")), b"tampered").unwrap();
        assert!(matches!(
            resolve_host_launch(HostLaunchContext {
                explicit_site: Some(site_path.clone()),
                executable_path: kit.join("Clew.exe"),
                state_layout: state.clone(),
                client_flavor: ClientFlavor::clew_original_current(),
                archive_temp_detected: false,
            }),
            Err(HostLaunchError::Outfit(OutfitError::AssetHashMismatch(_)))
        ));

        std::fs::remove_file(asset_dir.join(format!("{asset_id}.svg"))).unwrap();
        assert!(matches!(
            resolve_host_launch(HostLaunchContext {
                explicit_site: Some(site_path),
                executable_path: kit.join("Clew.exe"),
                state_layout: state,
                client_flavor: ClientFlavor::clew_original_current(),
                archive_temp_detected: false,
            }),
            Err(HostLaunchError::MissingOutfitAsset(_))
        ));
    }

    #[test]
    fn fixed_sibling_nearby_file_is_imported_for_the_matching_site() {
        let temp = tempdir().unwrap();
        let kit = temp.path().join("kit");
        std::fs::create_dir_all(&kit).unwrap();
        let state = StateLayout::new(temp.path().join("state"));
        let controller = ControllerIdentity::from_secret([94_u8; 32]);
        let site_id = SiteId::new();
        let invite_id = InviteId::new();
        let mut registry = EnrollmentRegistry::new(
            controller.controller_id(),
            PermissionGrant::EXECUTE_READ_CONNECTOR,
        );
        let pass = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id,
                    invite_id,
                    site_name: "Nearby Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 86_400_000,
                    deployment_window_ms: 60_000,
                    max_claims: 2,
                },
            )
            .unwrap();
        let bogus_controller = EndpointAddr::new(SecretKey::from_bytes(&[95_u8; 32]).public());
        let site = SignedSiteClew::issue_networked_outfit_sealed(
            &controller,
            OutfitProfile::preset(OutfitPreset::ClewOriginal),
            pass,
            HostRoleHint::ExecutePreferred,
            bogus_controller,
            clew_core::ReadPolicy::new(vec![kit.to_string_lossy().into_owned()], 4096, 2_000)
                .unwrap(),
            noise_static_public([96_u8; 32]).unwrap(),
        )
        .unwrap();
        site.write(&kit.join("site.clew")).unwrap();

        let helper_endpoint = SecretKey::from_bytes(&[97_u8; 32]).public();
        let mut helper_addr = EndpointAddr::new(helper_endpoint);
        helper_addr
            .addrs
            .insert(TransportAddr::Ip("127.0.0.1:4545".parse().unwrap()));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            clew_core::DeviceId::new(),
            helper_endpoint,
            now.saturating_sub(1_000),
            now + 60_000,
        )
        .unwrap();
        let nearby = NearbyConnectorFile::from_helper(helper_addr.clone(), lease).unwrap();
        std::fs::write(
            kit.join(NEARBY_CONNECTOR_FILE_NAME),
            nearby.to_bytes().unwrap(),
        )
        .unwrap();

        let result = resolve_host_launch(HostLaunchContext {
            explicit_site: None,
            executable_path: kit.join(if cfg!(windows) { "Clew.exe" } else { "Clew" }),
            state_layout: state.clone(),
            client_flavor: ClientFlavor::clew_original_current(),
            archive_temp_detected: false,
        })
        .unwrap();
        assert!(matches!(result, HostLaunchState::AwaitingEnrollment { .. }));
        let imported = NearbyConnectorStore::new(state)
            .load_import(&controller.public_identity(), site_id)
            .unwrap()
            .expect("fixed sibling nearby file was not imported");
        assert_eq!(imported.candidate, helper_addr);
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
        assert!(
            view.extract_first
                .unwrap()
                .contains("Extract the complete archive")
        );
    }
}
