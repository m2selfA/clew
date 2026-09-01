#![forbid(unsafe_code)]

mod instance;
mod membership;
mod naming;
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
pub use read::HostReadService;
pub use remote::{
    HostRemoteError, complete_networked_activation, serve_networked_membership_once,
    serve_networked_membership_until,
};
pub use runtime::{
    HostLaunchContext, HostLaunchError, HostLaunchState, HostSiteSource, MissingInviteView,
    detect_archive_temp_launch, resolve_host_launch,
};
pub use selection::{DeviceSelectionError, select_executable_device};
pub use site::{
    ClientFlavor, ClientFlavorId, HostRoleHint, SignedSiteClew, SiteClewError, SiteKitContract,
    TargetPlatform,
};
pub use ui::{OutfitRuntimeView, UiResources};
