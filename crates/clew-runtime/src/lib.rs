#![forbid(unsafe_code)]

mod backup;
mod config;
mod control;
mod controller;
mod local_api;
mod lock;
mod outfit;
mod remote;
mod transport;

pub use backup::{ControllerBackupIoError, export_controller_backup, restore_controller_backup};
pub use config::{ControllerConfig, ControllerConfigError, LocalEndpoint, default_state_root};
pub use control::{ControlStoreError, ControllerControlSnapshot, ControllerControlStore};
pub use controller::{ControllerError, ControllerRuntime, ControllerStart, start_controller};
pub use local_api::{
    ActivityList, BackupExportRequest, ControllerStatus, DeviceList, InviteIssueRequest,
    InviteIssueResult, LOCAL_API_VERSION, LocalApiClient, LocalApiClientError, LocalApiErrorCode,
    MAX_LOCAL_API_CONNECTIONS, MAX_LOCAL_API_FRAME_SIZE, OutfitCloneRequest, OutfitCreateRequest,
    OutfitList, OutfitSetFieldRequest, RecoveryStatus, RemoteReadRequest, RemoteReadResult,
};
pub use outfit::{
    MAX_CUSTOM_OUTFITS, OutfitLibrary, OutfitLibraryEntry, OutfitLibrarySnapshot, OutfitStoreError,
};
pub use remote::{
    MAX_REMOTE_CONNECTIONS, RemoteConnectionError, RemoteHub, RemoteHubError,
    handle_remote_connection,
};
