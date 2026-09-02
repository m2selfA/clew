#![forbid(unsafe_code)]

mod instance;
mod membership;
mod naming;
mod outfit;
mod read;
mod remote;
mod runtime;
mod selection;
mod site;
mod ui;

pub use instance::{HostInstance, HostInstanceKey, HostInstanceStart, acquire_host_instance};
pub use membership::{
    HostMembership, HostMembershipError, HostMembershipMarker, HostMembershipStore,
};
pub use naming::{
    HostNamingError, apply_hostname_collision_policy, normalize_hostname, observed_hostname,
};
pub use outfit::{
    KEY_AWAITING_ENROLLMENT, KEY_CHOOSE_INVITE, KEY_EXIT_AND_DISCONNECT, KEY_EXTRACT_FIRST,
    KEY_HELPER_READY, KEY_HIDE_TO_TRAY, KEY_MISSING_INVITE_BODY, KEY_MISSING_INVITE_TITLE,
    KEY_READY, KEY_TRAY_CONNECTED, KEY_TRAY_EXIT, KEY_TRAY_RECONNECTING, KEY_TRAY_SHOW,
    MAX_OUTFIT_ASSET_BYTES, MAX_OUTFIT_ENCODED_BYTES, OUTFIT_SCHEMA_VERSION, OutfitAssetRef,
    OutfitDistributionCopy, OutfitError, OutfitIdentity, OutfitPreset, OutfitProfile,
    OutfitStrings, OutfitVisuals, SurfaceStyle, outfit_asset_id_for_bytes,
    verify_outfit_asset_bytes,
};
pub use read::HostReadService;
pub use remote::{
    HostRemoteError, complete_networked_activation, serve_networked_membership_once,
    serve_networked_membership_until,
};
pub use runtime::{
    HostLaunchContext, HostLaunchError, HostLaunchState, HostSiteSource, MissingInviteView,
    cached_outfit_asset_path, detect_archive_temp_launch, resolve_host_launch,
};
pub use selection::{DeviceSelectionError, select_executable_device};
pub use site::{
    ClientFlavor, ClientFlavorId, HostRoleHint, SignedSiteClew, SiteClewError, SiteKitContract,
    TargetPlatform,
};
pub use ui::{OutfitRuntimeView, UiResources};
