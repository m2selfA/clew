#![forbid(unsafe_code)]

mod config;
mod controller;
mod local_api;
mod lock;
mod transport;

pub use config::{ControllerConfig, ControllerConfigError, LocalEndpoint, default_state_root};
pub use controller::{ControllerError, ControllerRuntime, ControllerStart, start_controller};
pub use local_api::{
    ControllerStatus, DeviceList, LOCAL_API_VERSION, LocalApiClient, LocalApiClientError,
    LocalApiErrorCode, MAX_LOCAL_API_CONNECTIONS, MAX_LOCAL_API_FRAME_SIZE,
};
