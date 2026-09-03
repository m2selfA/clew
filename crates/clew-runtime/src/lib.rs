#![forbid(unsafe_code)]

mod backup;
mod config;
mod control;
mod controller;
mod forward;
mod local_api;
mod lock;
mod outfit;
mod outfit_asset;
mod remote;
mod socks5;
mod transport;

pub use backup::{ControllerBackupIoError, export_controller_backup, restore_controller_backup};
pub use clew_transport::{FsMutationResult, FsWritePrecondition, ShellTaskOutput, ShellTaskStatus};
pub use config::{ControllerConfig, ControllerConfigError, LocalEndpoint, default_state_root};
pub use control::{ControlStoreError, ControllerControlSnapshot, ControllerControlStore};
pub use controller::{ControllerError, ControllerRuntime, ControllerStart, start_controller};
pub use forward::{
    ForwardInfo, HARD_MAX_FORWARD_LISTENERS, TcpForwardManager, TcpForwardManagerError,
};
pub use local_api::{
    ActivityList, BackupExportRequest, ControllerStatus, DeviceList, ForwardAddRequest,
    ForwardList, InviteIssueRequest, InviteIssueResult, LOCAL_API_VERSION, LocalApiClient,
    LocalApiClientError, LocalApiErrorCode, MAX_LOCAL_API_CONNECTIONS, MAX_LOCAL_API_FRAME_SIZE,
    OutfitAssetDataResponse, OutfitAssetImportRequest, OutfitAssetList, OutfitAssetPreviewResponse,
    OutfitCloneRequest, OutfitCreateRequest, OutfitList, OutfitSetAssetRequest,
    OutfitSetFieldRequest, OutfitUpdateRequest, RecoveryStatus, RemoteEditRequest,
    RemoteGlobRequest, RemoteGrepRequest, RemotePathInfoRequest, RemoteReadRequest,
    RemoteReadResult, RemoteSessionPathInfo, RemoteShellAttachRequest, RemoteShellStartRequest,
    RemoteWriteRequest, Socks5AddRequest, Socks5List,
};
pub use outfit::{
    MAX_CUSTOM_OUTFITS, OutfitEditPatch, OutfitLibrary, OutfitLibraryEntry, OutfitLibrarySnapshot,
    OutfitStoreError,
};
pub use outfit_asset::{
    MAX_OUTFIT_ASSET_BYTES, MAX_OUTFIT_ASSET_TOTAL_BYTES, MAX_OUTFIT_ASSETS,
    MAX_OUTFIT_PREVIEW_EDGE, OutfitAssetData, OutfitAssetError, OutfitAssetFormat, OutfitAssetInfo,
    OutfitAssetPreview, OutfitAssetStore,
};
pub use remote::{
    MAX_REMOTE_CONNECTIONS, RemoteConnectionError, RemoteHub, RemoteHubError, RemotePathState,
    RemoteSessionInfo, RemoteSessionState, RemoteSessionTopology, handle_remote_connection,
};
pub use socks5::{
    HARD_MAX_SOCKS5_LISTENERS, Socks5Info, Socks5ProxyManager, Socks5ProxyManagerError,
};
