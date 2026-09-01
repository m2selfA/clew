#![forbid(unsafe_code)]

mod device;
mod id;
mod naming;
mod state;

pub use device::{DeviceNameOrigin, DeviceRecord, DeviceSummary, MemberCapabilities, SiteMember};
pub use id::{ControllerId, DeviceId, InviteId, SiteId, StableIdError};
pub use naming::{
    DeviceTag, DeviceTagAllocation, DeviceTagAllocationError, DeviceTagParseError,
    allocate_device_tag,
};
pub use state::{
    MAX_STATE_DOCUMENT_SIZE, STATE_SCHEMA_VERSION, StateCodecError, StateEnvelope, StateLayout,
    decode_state_json, encode_state_json,
};
