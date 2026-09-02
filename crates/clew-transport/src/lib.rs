#![forbid(unsafe_code)]

mod connector;
mod discovery;
mod inner;
mod lease;
mod nearby;
mod outer;
mod protocol;
mod sealed_bootstrap;

pub use connector::{
    CONNECTOR_CONTROL_VERSION, ConnectorControlError, ConnectorOpenRequest, ConnectorReady,
    ConnectorTunnelPurpose, MAX_CONNECTOR_CONTROL_FRAME_BYTES, OpaqueForwardStats,
    forward_opaque_bidirectional, read_connector_open, read_connector_ready, write_connector_open,
    write_connector_ready,
};
pub use discovery::{
    ConnectorCandidate, ConnectorDiscoveryAdvertisement, ConnectorDiscoveryError,
    ConnectorDiscoveryEvent, ConnectorDiscoveryEvents, MdnsConnectorDiscovery, SiteDiscoveryTag,
};
pub use inner::{
    ControllerSessionAuthority, ControllerSessionIdentity, DeviceSessionClaim,
    DeviceSessionIdentity, InnerMessage, InnerRole, InnerSession, InnerSessionError,
    MAX_INNER_PLAINTEXT,
};
pub use lease::{
    CONNECTOR_LEASE_MESSAGE_KIND, CONNECTOR_LEASE_VERSION, ConnectorLease, ConnectorLeaseError,
    ConnectorLeaseRole, MAX_CONNECTOR_LEASE_ENCODED_BYTES, MAX_CONNECTOR_LEASE_LIFETIME_MS,
    SignedConnectorLease,
};
pub use nearby::{
    MAX_NEARBY_CONNECTOR_ADDRS, MAX_NEARBY_CONNECTOR_FILE_BYTES, NEARBY_CONNECTOR_FILE_KIND,
    NEARBY_CONNECTOR_FILE_VERSION, NearbyConnectorError, NearbyConnectorFile,
};
pub use outer::{IrohOuter, IrohOuterError, IrohProtocol, IrohStream};
pub use protocol::{
    BootstrapErrorBody, BootstrapErrorCode, BootstrapMemberMode, BootstrapProtocolError,
    BootstrapRequest, BootstrapResponse, MAX_BOOTSTRAP_FRAME_BYTES, ReadErrorBody, ReadErrorCode,
    ReadProtocolError, ReadReply, ReadRequest, read_bootstrap, write_bootstrap,
};
pub use sealed_bootstrap::{
    MAX_SEALED_BOOTSTRAP_PLAINTEXT, SealedBootstrapContext, SealedBootstrapError,
    SealedBootstrapSession, noise_static_public,
};
