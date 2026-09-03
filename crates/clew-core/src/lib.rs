#![forbid(unsafe_code)]

mod control;
mod device;
mod id;
mod naming;
mod selection;
mod state;

pub use control::{
    ActivityEvent, ActivityResult, ControlModelError, ControllerCatalog, ControllerDeviceRecord,
    ControllerSiteRecord, HARD_ACTIVITY_RETENTION_MS, HARD_MAX_ACTIVITY_EVENTS,
    HARD_MAX_READ_RESULT_BYTES, HARD_MAX_READ_ROOT_BYTES, HARD_MAX_READ_ROOTS,
    HARD_MAX_READ_TIMEOUT_MS, ReadPolicy,
};
pub use device::{DeviceNameOrigin, DeviceRecord, DeviceSummary, MemberCapabilities, SiteMember};
pub use id::{
    ControllerId, DeviceId, ForwardConnectionId, ForwardId, InviteId, RequestId, SiteId,
    StableIdError, TaskId, TransferId,
};
pub use naming::{
    DeviceTag, DeviceTagAllocation, DeviceTagAllocationError, DeviceTagParseError,
    allocate_device_tag,
};
pub use selection::{DeviceSelectionError, select_executable_device};
pub use state::{
    MAX_STATE_DOCUMENT_SIZE, STATE_SCHEMA_VERSION, StateCodecError, StateEnvelope, StateLayout,
    decode_state_json, encode_state_json,
};
