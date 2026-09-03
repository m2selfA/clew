#![forbid(unsafe_code)]

mod connector;
mod discovery;
mod file_resume;
mod file_transfer;
mod fs_mutation;
mod fs_query;
mod inner;
mod lease;
mod nearby;
mod outer;
mod protocol;
mod rpc;
mod sealed_bootstrap;
mod shell_task;
mod tcp_forward;

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
pub use file_resume::{
    EMPTY_SHA256_HEX, FILE_RESUME_DESCRIPTOR_VERSION, FileResumeDescriptor, FileResumeError,
    FileTransferDirection, MAX_FILE_RESUME_DESCRIPTOR_BYTES, MAX_FILE_RESUME_PATH_BYTES,
};
pub use file_transfer::{
    FILE_TRANSFER_MANIFEST_VERSION, FileConflictPolicy, FileTransferChunk, FileTransferError,
    FileTransferErrorBody, FileTransferErrorCode, FileTransferManifest, FileTransferPhase,
    FileTransferReply, FileTransferRequest, FileTransferStatus, MAX_FILE_CHUNK_BASE64_BYTES,
    MAX_FILE_CHUNK_BYTES, MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES, MAX_FILE_TRANSFER_MANIFEST_BYTES,
    MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES, MIN_FILE_CHUNK_BYTES, sha256_hex as file_sha256_hex,
};
pub use fs_mutation::{
    FsMutationErrorBody, FsMutationErrorCode, FsMutationProtocolError, FsMutationReply,
    FsMutationRequest, FsMutationResult, FsWritePrecondition, HARD_MAX_EDIT_FRAGMENT_BYTES,
    HARD_MAX_WRITE_TEXT_BYTES, normalize_sha256_hex,
};
pub use fs_query::{
    FsGlobPage, FsGrepMatch, FsGrepPage, FsPathInfo, FsPathKind, FsQueryErrorBody,
    FsQueryErrorCode, FsQueryProtocolError, FsQueryReply, FsQueryRequest,
    HARD_MAX_FS_PATTERN_BYTES, HARD_MAX_FS_RESULT_ITEMS, HARD_MAX_FS_SCAN_ENTRIES,
    HARD_MAX_GREP_LINE_BYTES, HARD_MAX_GREP_SCAN_BYTES,
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
pub use rpc::{
    RPC_PROGRESS_INTERVAL_MS, RPC_PROGRESS_MESSAGE_KIND, RPC_REPLY_MESSAGE_KIND,
    RPC_REQUEST_MESSAGE_KIND, RpcProtocolError, is_rpc_progress, unwrap_rpc_reply,
    unwrap_rpc_request, wrap_rpc_progress, wrap_rpc_reply, wrap_rpc_request,
};
pub use sealed_bootstrap::{
    MAX_SEALED_BOOTSTRAP_PLAINTEXT, SealedBootstrapContext, SealedBootstrapError,
    SealedBootstrapSession, noise_static_public,
};
pub use shell_task::{
    HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM, HARD_MAX_SHELL_COMMAND_BYTES,
    HARD_MAX_SHELL_ENV_ENTRIES, HARD_MAX_SHELL_ENV_KEY_BYTES, HARD_MAX_SHELL_ENV_TOTAL_BYTES,
    HARD_MAX_SHELL_ENV_VALUE_BYTES, HARD_MAX_SHELL_RETAINED_BYTES_PER_STREAM,
    HARD_MAX_SHELL_TASKS_PER_SESSION, HARD_MAX_SHELL_TIMEOUT_MS, SHELL_RECONNECT_GRACE_MS,
    ShellOutputChunk, ShellTaskErrorBody, ShellTaskErrorCode, ShellTaskOutput, ShellTaskPhase,
    ShellTaskProtocolError, ShellTaskReply, ShellTaskRequest, ShellTaskStatus,
};
pub use tcp_forward::{
    HARD_MAX_TCP_FORWARD_CHUNK_BYTES, HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS,
    HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION, HARD_MAX_TCP_FORWARD_HOST_BYTES,
    HARD_MAX_TCP_FORWARD_READ_WAIT_MS, TcpForwardDestination, TcpForwardErrorBody,
    TcpForwardErrorCode, TcpForwardProtocolError, TcpForwardReply, TcpForwardRequest,
};
