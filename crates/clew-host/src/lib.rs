#![forbid(unsafe_code)]

mod directory_tree;
mod file_transfer;
mod forward;
mod instance;
mod membership;
mod naming;
mod nearby;
mod outfit;
mod read;
mod remote;
mod runtime;
mod shell;
mod site;
mod ui;

pub use clew_core::{DeviceSelectionError, select_executable_device};
pub use directory_tree::{
    DirectoryTreeScanError, HostDirectoryTreeService, ScannedDirectoryTree,
    scan_authorized_directory_tree, scan_directory_tree,
};
pub use file_transfer::{
    HARD_MAX_HOST_FILE_TRANSFERS, HostFileTransferService, HostFileTransferStateError,
};
pub use forward::HostTcpForwardService;
pub use instance::{HostInstance, HostInstanceKey, HostInstanceStart, acquire_host_instance};
pub use membership::{
    HostMembership, HostMembershipError, HostMembershipMarker, HostMembershipStore,
};
pub use naming::{
    HostNamingError, apply_hostname_collision_policy, normalize_hostname, observed_hostname,
};
pub use nearby::{
    LEGACY_NEARBY_CONNECTOR_FILE_NAME, NEARBY_CONNECTOR_FILE_NAME, NearbyConnectorStore,
    NearbyConnectorStoreError,
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
    serve_networked_membership_once_with_layout, serve_networked_membership_until,
    serve_networked_membership_until_with_layout, wait_for_networked_activation_until,
};
pub use runtime::{
    HostLaunchContext, HostLaunchError, HostLaunchMode, HostLaunchState, HostSiteSource,
    MissingInviteView, cached_outfit_asset_path, detect_archive_temp_launch, resolve_host_launch,
    resolve_host_launch_with_mode,
};
pub use shell::HostShellService;
pub use site::{
    ClientFlavor, ClientFlavorId, HostRoleHint, SignedSiteClew, SiteClewError, SiteKitContract,
    TargetPlatform,
};
pub use ui::{OutfitRuntimeView, UiResources};
