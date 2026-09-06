use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    net::SocketAddr,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_core::{
    DeviceId, HARD_MAX_READ_RESULT_BYTES, TaskId, TransferId, select_executable_device,
};
use clew_runtime::{
    ControllerConfig, DirectoryGetInfo, DirectoryPutInfo, DirectoryTransferInfo,
    FileConflictPolicy, FileGetInfo, FilePutInfo, FileTransferInfo, FsManagedTempKind,
    FsMutationRequest, FsWritePrecondition, LocalApiClient, RemoteDirectoryGetRequest,
    RemoteDirectoryPutRequest, RemoteEditRequest, RemoteFileGetRequest, RemoteFilePutRequest,
    RemoteFsControlRequest, RemoteGlobRequest, RemoteGrepRequest, RemotePathInfoRequest,
    RemoteReadRequest, RemoteShellAttachRequest, RemoteShellStartRequest, RemoteWriteRequest,
};
use clew_transport::{
    HARD_MAX_FS_RESULT_ITEMS, HARD_MAX_GREP_SCAN_BYTES, HARD_MAX_MANAGED_TEMP_TTL_MS,
    HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM, HARD_MAX_SHELL_TIMEOUT_MS, MAX_FILE_CHUNK_BYTES,
    MIN_FILE_CHUNK_BYTES,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars::{self, JsonSchema},
    tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const MCP_DEFAULT_READ_LIMIT: u32 = 16_384;
const MCP_DEFAULT_PAGE_LIMIT: u32 = 128;
const MCP_DEFAULT_RESULT_BYTES: u32 = 32_768;
const MCP_DEFAULT_GREP_SCAN_BYTES: u64 = 8_388_608;
const MCP_DEFAULT_SHELL_TIMEOUT_MS: u32 = 300_000;
const MCP_DEFAULT_SHELL_ATTACH_BYTES: u32 = 12_288;
const MCP_DEFAULT_FILE_CHUNK_BYTES: u32 = 32_768;
const MCP_MAX_TRANSFER_BATCH_ITEMS: usize = 8;
const MCP_MAX_TRANSFER_STATUS_ITEMS: usize = 32;
const MCP_DEFAULT_TEMP_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
const MCP_ERROR_TEXT_BYTES: usize = 2_048;
const MCP_CONTROLLER_START_TIMEOUT: Duration = Duration::from_secs(8);
const MCP_HTTP_MAX_REQUEST_BODY_BYTES: usize = 128 * 1024;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
struct ClewMcpServer {
    client: LocalApiClient,
}

impl ClewMcpServer {
    fn new(config: ControllerConfig) -> Self {
        Self {
            client: LocalApiClient::new(config),
        }
    }

    async fn resolve_device(&self, selector: Option<&str>) -> Result<DeviceId, CallToolResult> {
        let devices = self
            .client
            .device_list()
            .await
            .map_err(tool_error_from_display)?;
        select_executable_device(&devices.devices, selector).map_err(tool_error_from_display)
    }

    async fn execute_fs_control(
        &self,
        selector: Option<&str>,
        request: FsMutationRequest,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(selector).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .fs_control(RemoteFsControlRequest { device_id, request })
            .await
        {
            Ok(result) => structured_result(result),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    async fn prepare_put_batch(
        &self,
        args: &TransferPutBatchArgs,
    ) -> Result<(DeviceId, Vec<PreparedPutBatchItem>, TransferVerbosity), CallToolResult> {
        validate_transfer_batch_len(args.items.len(), MCP_MAX_TRANSFER_BATCH_ITEMS)?;
        let device_id = self.resolve_device(args.device.as_deref()).await?;
        let mut targets = BTreeSet::new();
        let mut prepared = Vec::with_capacity(args.items.len());
        for (index, item) in args.items.iter().enumerate() {
            if !targets.insert(item.target_path.clone()) {
                return Err(tool_error(format!(
                    "transfer batch item {index} repeats target_path {}",
                    item.target_path
                )));
            }
            let source = std::fs::canonicalize(&item.source_path).map_err(|error| {
                tool_error(format!(
                    "transfer batch item {index} Controller-A source_path is unavailable: {error}"
                ))
            })?;
            let Some(source_path) = source.to_str().map(str::to_owned) else {
                return Err(tool_error(format!(
                    "transfer batch item {index} Controller-A source_path must be valid UTF-8"
                )));
            };
            let chunk_size = checked_transfer_chunk_size(item.chunk_size)?;
            if item.recursive {
                if !source.is_dir() {
                    return Err(tool_error(format!(
                        "transfer batch item {index} has recursive=true but source_path is not a directory"
                    )));
                }
                if !matches!(item.conflict, TransferConflictMode::Fail) {
                    return Err(tool_error(format!(
                        "transfer batch item {index} is recursive; directory transfer requires conflict=fail"
                    )));
                }
                prepared.push(PreparedPutBatchItem::Directory {
                    source_path,
                    target_root: item.target_path.clone(),
                    chunk_size,
                });
            } else {
                if !source.is_file() {
                    return Err(tool_error(format!(
                        "transfer batch item {index} is not a regular file; set recursive=true for a directory tree"
                    )));
                }
                prepared.push(PreparedPutBatchItem::File {
                    source_path,
                    target_path: item.target_path.clone(),
                    chunk_size,
                    conflict: item.conflict,
                });
            }
        }
        Ok((
            device_id,
            prepared,
            args.verbosity.unwrap_or(TransferVerbosity::Summary),
        ))
    }

    async fn prepare_get_batch(
        &self,
        args: &TransferGetBatchArgs,
    ) -> Result<(DeviceId, Vec<PreparedGetBatchItem>, TransferVerbosity), CallToolResult> {
        validate_transfer_batch_len(args.items.len(), MCP_MAX_TRANSFER_BATCH_ITEMS)?;
        let device_id = self.resolve_device(args.device.as_deref()).await?;
        let mut destinations = BTreeSet::new();
        let mut prepared = Vec::with_capacity(args.items.len());
        for (index, item) in args.items.iter().enumerate() {
            let destination_path = controller_destination_path(&item.destination_path)?;
            if !destinations.insert(destination_path.clone()) {
                return Err(tool_error(format!(
                    "transfer batch item {index} repeats Controller-A destination_path {destination_path}"
                )));
            }
            if item.recursive && !matches!(item.conflict, TransferConflictMode::Fail) {
                return Err(tool_error(format!(
                    "transfer batch item {index} is recursive; directory transfer requires conflict=fail"
                )));
            }
            if item.recursive && std::path::Path::new(&destination_path).exists() {
                return Err(tool_error(format!(
                    "transfer batch item {index} recursive destination already exists on Controller A"
                )));
            }
            let info = self
                .client
                .path_info(RemotePathInfoRequest {
                    device_id,
                    path: item.target_path.clone(),
                })
                .await
                .map_err(|error| {
                    tool_error(format!(
                        "transfer batch item {index} target preflight failed: {error}"
                    ))
                })?;
            let kind = serde_json::to_value(info.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into());
            let expected_kind = if item.recursive { "directory" } else { "file" };
            if kind != expected_kind {
                return Err(tool_error(format!(
                    "transfer batch item {index} expected target kind {expected_kind}, got {kind}; set recursive to match the source"
                )));
            }
            let chunk_size = checked_transfer_chunk_size(item.chunk_size)?;
            if item.recursive {
                prepared.push(PreparedGetBatchItem::Directory {
                    target_root: item.target_path.clone(),
                    destination_path,
                    chunk_size,
                });
            } else {
                prepared.push(PreparedGetBatchItem::File {
                    target_path: item.target_path.clone(),
                    destination_path,
                    chunk_size,
                    conflict: item.conflict,
                });
            }
        }
        Ok((
            device_id,
            prepared,
            args.verbosity.unwrap_or(TransferVerbosity::Summary),
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReadOutput {
    #[default]
    Auto,
    Text,
    Base64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadArgs {
    #[serde(default)]
    #[schemars(
        description = "Optional DeviceId, Site/Device, or unique executable short name. Omit only when exactly one online executable device exists."
    )]
    device: Option<String>,
    #[schemars(
        description = "Path on target B allowed by the signed Site read policy. Use an absolute path or ~/... for the home directory of the OS account running Clew on B."
    )]
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    #[schemars(
        description = "Maximum raw bytes to return. Defaults to 16384; values above Clew's 49152-byte hard result bound are safely capped and larger files should be paged with offset."
    )]
    limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Output representation: auto (default) returns UTF-8 text when valid and base64 otherwise; text requires valid UTF-8; base64 always preserves raw bytes."
    )]
    output: Option<ReadOutput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PathInfoArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Path on target B allowed by the signed Site read policy. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GlobArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Signed root or subdirectory on target B to search. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    root: String,
    #[schemars(description = "Relative bounded glob pattern, for example **/*.rs.")]
    pattern: String,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    #[schemars(
        description = "Maximum result items; values above the protocol hard bound are capped."
    )]
    limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Maximum encoded result budget; values above 49152 bytes are capped."
    )]
    max_bytes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrepArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Signed root or file on target B to search. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    root: String,
    #[schemars(
        description = "Rust regex pattern evaluated by Clew's bounded linear-time regex engine."
    )]
    pattern: String,
    #[serde(default)]
    #[schemars(description = "Optional relative glob filter such as **/*.rs.")]
    include: Option<String>,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    #[schemars(
        description = "Maximum result items; values above the protocol hard bound are capped."
    )]
    limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Maximum encoded result budget; values above 49152 bytes are capped."
    )]
    max_bytes: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Maximum bytes scanned; values above Clew's bounded grep hard limit are capped."
    )]
    max_scan_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    CreateOnly,
    MatchSha256,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Destination path on target B. Accepts an absolute path or ~/... and remains subject to signed Write authority and filesystem scope."
    )]
    path: String,
    contents: String,
    mode: WriteMode,
    #[serde(default)]
    #[schemars(
        description = "Required only for mode=match_sha256. Exactly 64 hexadecimal characters."
    )]
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Existing UTF-8 file on target B. Accepts an absolute path or ~/... and remains subject to signed Write authority and filesystem scope."
    )]
    path: String,
    expected_sha256: String,
    #[schemars(description = "Text that must occur exactly once in the current UTF-8 file.")]
    old: String,
    new: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShellStartArgs {
    #[serde(default)]
    device: Option<String>,
    command: String,
    #[schemars(
        description = "Initial working directory inside the signed filesystem scope. Accepts an absolute path or ~/... for B's Clew runtime account. This is not a filesystem sandbox."
    )]
    cwd: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    #[schemars(
        description = "Task timeout in milliseconds. Defaults to 300000 and is hard-bounded by the Shell protocol."
    )]
    timeout_ms: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShellTaskArgs {
    #[schemars(
        description = "TaskId returned by shell_start in the current live Controller/Target session."
    )]
    task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShellAttachArgs {
    task_id: String,
    #[serde(default)]
    stdout_offset: Option<u64>,
    #[serde(default)]
    stderr_offset: Option<u64>,
    #[serde(default)]
    #[schemars(
        description = "Maximum stdout/stderr bytes per attach call; values above the Shell hard bound are capped."
    )]
    max_bytes_per_stream: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TransferConflictMode {
    #[default]
    Fail,
    Replace,
    Rename,
}

impl From<TransferConflictMode> for FileConflictPolicy {
    fn from(value: TransferConflictMode) -> Self {
        match value {
            TransferConflictMode::Fail => Self::FailIfExists,
            TransferConflictMode::Replace => Self::ReplaceExisting,
            TransferConflictMode::Rename => Self::RenameIfExists,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TransferVerbosity {
    Minimal,
    Summary,
    Full,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransferReferenceKind {
    File,
    Directory,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferReferenceArg {
    kind: TransferReferenceKind,
    #[schemars(description = "Durable TransferId returned by a Clew file or directory transfer.")]
    transfer_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferBatchStatusArgs {
    #[schemars(
        description = "One or more durable transfer references. At most 32 may be queried per call."
    )]
    transfers: Vec<TransferReferenceArg>,
    #[serde(default)]
    #[schemars(
        description = "minimal returns aggregate state plus ids; summary adds progress; full returns complete underlying transfer records."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferPutBatchItem {
    #[schemars(
        description = "Controller-A local source path. Files use the single-file V5 plane; directories require recursive=true and use the existing recursive directory plane."
    )]
    source_path: String,
    #[schemars(
        description = "Destination path/root on target B. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    target_path: String,
    #[serde(default)]
    #[schemars(
        description = "Set true only for a directory tree. Recursive items reuse Clew's existing bounded directory-transfer implementation."
    )]
    recursive: bool,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Single-file conflict policy. Recursive directory transfers are always fail-if-exists and therefore require conflict=fail."
    )]
    conflict: TransferConflictMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferPutBatchArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "1..=8 independent upload roots. Clew preflights every Controller-A source before starting any transfer."
    )]
    items: Vec<TransferPutBatchItem>,
    #[serde(default)]
    #[schemars(description = "Defaults to summary for batch operations.")]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferGetBatchItem {
    #[schemars(
        description = "Source path/root on target B. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    target_path: String,
    #[schemars(description = "Controller-A local destination path/root.")]
    destination_path: String,
    #[serde(default)]
    #[schemars(
        description = "Set true to download a directory tree recursively using the existing bounded directory-transfer plane; false means one regular file."
    )]
    recursive: bool,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Single-file destination conflict policy. Recursive directory transfers are fail-if-exists and require conflict=fail."
    )]
    conflict: TransferConflictMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferGetBatchArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "1..=8 independent download roots. Clew preflights all B-side source kinds and Controller-A destinations before starting any transfer."
    )]
    items: Vec<TransferGetBatchItem>,
    #[serde(default)]
    #[schemars(description = "Defaults to summary for batch operations.")]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug)]
enum PreparedPutBatchItem {
    File {
        source_path: String,
        target_path: String,
        chunk_size: u32,
        conflict: TransferConflictMode,
    },
    Directory {
        source_path: String,
        target_root: String,
        chunk_size: u32,
    },
}

#[derive(Debug)]
enum PreparedGetBatchItem {
    File {
        target_path: String,
        destination_path: String,
        chunk_size: u32,
        conflict: TransferConflictMode,
    },
    Directory {
        target_root: String,
        destination_path: String,
        chunk_size: u32,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FilePutArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Path on Controller A. Clew reads this local file and uploads it to B."
    )]
    source_path: String,
    #[schemars(
        description = "Destination file path on target B, subject to B's signed filesystem policy. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    target_path: String,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    conflict: TransferConflictMode,
    #[serde(default)]
    #[schemars(
        description = "Optional result verbosity. Defaults to full for the existing single-file tool."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FileGetArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Source file path on target B, subject to B's signed filesystem policy. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    target_path: String,
    #[schemars(
        description = "Destination path on Controller A. Relative paths are resolved against Clew's Controller working directory."
    )]
    destination_path: String,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    conflict: TransferConflictMode,
    #[serde(default)]
    #[schemars(
        description = "Optional result verbosity. Defaults to full for the existing single-file tool."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DirectoryPutArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(description = "Directory on Controller A to upload recursively.")]
    source_path: String,
    #[schemars(
        description = "New directory root on target B. Accepts an absolute path or ~/...; Directory Put is fail-if-exists."
    )]
    target_root: String,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional result verbosity. Defaults to full for the existing recursive directory tool."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DirectoryGetArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Directory root on target B to download recursively. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    target_root: String,
    #[schemars(
        description = "New destination directory on Controller A. Directory Get is fail-if-exists."
    )]
    destination_path: String,
    #[serde(default)]
    chunk_size: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional result verbosity. Defaults to full for the existing recursive directory tool."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TransferIdArgs {
    #[schemars(description = "TransferId returned by a file or directory transfer tool.")]
    transfer_id: String,
    #[serde(default)]
    #[schemars(
        description = "Optional result verbosity. Defaults to full for the existing status/cancel tools."
    )]
    verbosity: Option<TransferVerbosity>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FsPathControlArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Path on target B inside its signed filesystem scope. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FsCopyMoveArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Existing source path on target B. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    source: String,
    #[schemars(
        description = "Destination path on target B; it must not already exist. Accepts an absolute path or ~/... for B's Clew runtime account."
    )]
    destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeviceOnlyArgs {
    #[serde(default)]
    device: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TrashIdArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(
        description = "Clew trash_id returned by fs_trash or trash_list, not an arbitrary OS Trash item id."
    )]
    trash_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TrashPurgeConfirmArgs {
    #[serde(default)]
    device: Option<String>,
    trash_id: String,
    #[schemars(
        description = "Short-lived confirmation_token returned by trash_purge_prepare for this exact trash_id."
    )]
    confirmation_token: String,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ManagedTempKindArg {
    File,
    Directory,
}

impl From<ManagedTempKindArg> for FsManagedTempKind {
    fn from(value: ManagedTempKindArg) -> Self {
        match value {
            ManagedTempKindArg::File => Self::File,
            ManagedTempKindArg::Directory => Self::Directory,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TempCreateArgs {
    #[serde(default)]
    device: Option<String>,
    kind: ManagedTempKindArg,
    #[schemars(
        description = "Required human-readable purpose/ownership note stored with the managed resource."
    )]
    description: String,
    #[serde(default)]
    #[schemars(
        description = "Lifetime in milliseconds. Defaults to 24 hours; values above Clew's managed-temp hard maximum are capped."
    )]
    ttl_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TempIdArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(description = "Clew managed resource_id returned by temp_create/temp_list.")]
    resource_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionPathInfoArgs {
    #[schemars(
        description = "Stable Clew DeviceId from devices. Required exactly because session telemetry also covers helper-only and offline devices, where executable short-name selection is intentionally not used."
    )]
    device_id: String,
}

#[tool_router]
impl ClewMcpServer {
    #[tool(
        description = "List Controller-known devices with Site/name/hostname/online/executable/connector state. Helper-only devices are visible here but never executable candidates."
    )]
    async fn devices(&self) -> Result<CallToolResult, McpError> {
        match self.client.device_list().await {
            Ok(devices) => structured_result(devices),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Show the current or most recent Controller session generation, topology, iroh path state, and transition timestamps for one stable DeviceId. Generation is monotonic only within the current Controller process and changes on reconnect; path changes inside one iroh connection do not change generation."
    )]
    async fn session_path_info(
        &self,
        Parameters(args): Parameters<SessionPathInfoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match args.device_id.parse::<DeviceId>() {
            Ok(device_id) => device_id,
            Err(_) => {
                return Ok(tool_error(
                    "device_id must be a canonical non-nil Clew DeviceId",
                ));
            }
        };
        match self.client.session_path_info(device_id).await {
            Ok(info) => structured_result(info),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Read one bounded byte range from an executable device. output=auto (default) returns UTF-8 text directly when valid and falls back to base64 for binary/non-UTF-8 data; output=text requires UTF-8; output=base64 always preserves raw bytes. Do not scan whole disks; use the signed roots and small ranges."
    )]
    async fn read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let offset = args.offset.unwrap_or(0);
        let requested_limit = args.limit.unwrap_or(MCP_DEFAULT_READ_LIMIT);
        let limit = match bounded_max_u32(requested_limit, HARD_MAX_READ_RESULT_BYTES, "read limit")
        {
            Ok(limit) => limit,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .read(RemoteReadRequest {
                device_id,
                path: args.path.clone(),
                offset,
                limit,
            })
            .await
        {
            Ok(result) => match read_output_value(
                device_id,
                &args.path,
                offset,
                result.data,
                args.output.unwrap_or_default(),
            ) {
                Ok(value) => structured_result(value),
                Err(message) => Ok(tool_error(message)),
            },
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Return bounded metadata for one path on an executable device, using the same signed-root policy as Read."
    )]
    async fn path_info(
        &self,
        Parameters(args): Parameters<PathInfoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .path_info(RemotePathInfoRequest {
                device_id,
                path: args.path,
            })
            .await
        {
            Ok(info) => structured_result(info),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "List bounded glob matches under a signed root with deterministic pagination. Prefer narrow roots and patterns."
    )]
    async fn glob(
        &self,
        Parameters(args): Parameters<GlobArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let limit = match bounded_max_u32(
            args.limit.unwrap_or(MCP_DEFAULT_PAGE_LIMIT),
            HARD_MAX_FS_RESULT_ITEMS,
            "glob limit",
        ) {
            Ok(limit) => limit,
            Err(error) => return Ok(error),
        };
        let max_bytes = match bounded_max_u32(
            args.max_bytes.unwrap_or(MCP_DEFAULT_RESULT_BYTES),
            HARD_MAX_READ_RESULT_BYTES,
            "glob max_bytes",
        ) {
            Ok(max_bytes) => max_bytes,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .glob(RemoteGlobRequest {
                device_id,
                root: args.root,
                pattern: args.pattern,
                cursor: args.cursor.unwrap_or(0),
                limit,
                max_bytes,
            })
            .await
        {
            Ok(page) => structured_result(page),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Search bounded UTF-8 lines with Clew's linear-time regex engine, pagination, scan-byte limits, and signed-root containment."
    )]
    async fn grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let limit = match bounded_max_u32(
            args.limit.unwrap_or(MCP_DEFAULT_PAGE_LIMIT),
            HARD_MAX_FS_RESULT_ITEMS,
            "grep limit",
        ) {
            Ok(limit) => limit,
            Err(error) => return Ok(error),
        };
        let max_bytes = match bounded_max_u32(
            args.max_bytes.unwrap_or(MCP_DEFAULT_RESULT_BYTES),
            HARD_MAX_READ_RESULT_BYTES,
            "grep max_bytes",
        ) {
            Ok(max_bytes) => max_bytes,
            Err(error) => return Ok(error),
        };
        let max_scan_bytes = match bounded_max_u64(
            args.max_scan_bytes.unwrap_or(MCP_DEFAULT_GREP_SCAN_BYTES),
            HARD_MAX_GREP_SCAN_BYTES,
            "grep max_scan_bytes",
        ) {
            Ok(max_scan_bytes) => max_scan_bytes,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .grep(RemoteGrepRequest {
                device_id,
                root: args.root,
                pattern: args.pattern,
                include: args.include,
                cursor: args.cursor.unwrap_or(0),
                limit,
                max_bytes,
                max_scan_bytes,
            })
            .await
        {
            Ok(page) => structured_result(page),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Create or atomically replace one bounded UTF-8 file. Requires signed write authority and an explicit create-only or SHA-256 precondition."
    )]
    async fn write(
        &self,
        Parameters(args): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let precondition = match (args.mode, args.expected_sha256) {
            (WriteMode::CreateOnly, None) => FsWritePrecondition::CreateOnly,
            (WriteMode::MatchSha256, Some(hash)) => FsWritePrecondition::MatchSha256(hash),
            (WriteMode::CreateOnly, Some(_)) => {
                return Ok(tool_error("create_only must not include expected_sha256"));
            }
            (WriteMode::MatchSha256, None) => {
                return Ok(tool_error("match_sha256 requires expected_sha256"));
            }
        };
        match self
            .client
            .write(RemoteWriteRequest {
                device_id,
                path: args.path,
                contents: args.contents,
                precondition,
            })
            .await
        {
            Ok(result) => structured_result(result),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Atomically replace one uniquely occurring UTF-8 text fragment under an expected SHA-256. Requires signed write authority."
    )]
    async fn edit(
        &self,
        Parameters(args): Parameters<EditArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .edit(RemoteEditRequest {
                device_id,
                path: args.path,
                expected_sha256: args.expected_sha256,
                old: args.old,
                new: args.new,
            })
            .await
        {
            Ok(result) => structured_result(result),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Upload one Controller-A local file to target B using Clew's resumable, hash-verified V5 file plane. Requires signed Write authority on B. Returns a TransferId; poll file_status for durable progress."
    )]
    async fn file_put(
        &self,
        Parameters(args): Parameters<FilePutArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let source = match std::fs::canonicalize(&args.source_path) {
            Ok(path) if path.is_file() => path,
            Ok(_) => {
                return Ok(tool_error(
                    "source_path on Controller A must be a regular file",
                ));
            }
            Err(error) => {
                return Ok(tool_error(format!(
                    "Controller-A source_path is unavailable: {error}"
                )));
            }
        };
        let Some(source_path) = source.to_str().map(str::to_owned) else {
            return Ok(tool_error("Controller-A source_path must be valid UTF-8"));
        };
        let chunk_size = match checked_transfer_chunk_size(args.chunk_size) {
            Ok(chunk_size) => chunk_size,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .file_put(RemoteFilePutRequest {
                device_id,
                source_path,
                device_path: args.target_path,
                chunk_size,
                conflict_policy: args.conflict.into(),
            })
            .await
        {
            Ok(info) => file_put_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Download one file from target B to Controller A using Clew's resumable, hash-verified V5 file plane. Requires signed Read authority on B. Returns a TransferId; poll file_status for durable progress."
    )]
    async fn file_get(
        &self,
        Parameters(args): Parameters<FileGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let destination = match controller_destination_path(&args.destination_path) {
            Ok(path) => path,
            Err(error) => return Ok(error),
        };
        let chunk_size = match checked_transfer_chunk_size(args.chunk_size) {
            Ok(chunk_size) => chunk_size,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .file_get(RemoteFileGetRequest {
                device_id,
                device_path: args.target_path,
                destination_path: destination,
                chunk_size,
                conflict_policy: args.conflict.into(),
            })
            .await
        {
            Ok(info) => file_get_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Return Controller-owned durable progress for one single-file transfer by TransferId."
    )]
    async fn file_status(
        &self,
        Parameters(args): Parameters<TransferIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let transfer_id = match parse_transfer_id(&args.transfer_id) {
            Ok(id) => id,
            Err(error) => return Ok(error),
        };
        match self.client.file_status(transfer_id).await {
            Ok(info) => file_transfer_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(description = "Cancel one Controller-owned single-file transfer by TransferId.")]
    async fn file_cancel(
        &self,
        Parameters(args): Parameters<TransferIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let transfer_id = match parse_transfer_id(&args.transfer_id) {
            Ok(id) => id,
            Err(error) => return Ok(error),
        };
        match self.client.file_cancel(transfer_id).await {
            Ok(info) => file_transfer_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Upload a bounded directory tree from Controller A to target B using Clew's existing resumable directory-transfer plane. The target root must not already exist."
    )]
    async fn directory_put(
        &self,
        Parameters(args): Parameters<DirectoryPutArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let source = match std::fs::canonicalize(&args.source_path) {
            Ok(path) if path.is_dir() => path,
            Ok(_) => {
                return Ok(tool_error(
                    "source_path on Controller A must be a directory",
                ));
            }
            Err(error) => {
                return Ok(tool_error(format!(
                    "Controller-A source_path is unavailable: {error}"
                )));
            }
        };
        let Some(source_path) = source.to_str().map(str::to_owned) else {
            return Ok(tool_error("Controller-A source_path must be valid UTF-8"));
        };
        let chunk_size = match checked_transfer_chunk_size(args.chunk_size) {
            Ok(chunk_size) => chunk_size,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .directory_put(RemoteDirectoryPutRequest {
                device_id,
                source_path,
                device_root: args.target_root,
                chunk_size,
            })
            .await
        {
            Ok(info) => directory_put_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Download a bounded directory tree from target B to Controller A. The Controller destination must not already exist; poll directory_status for progress."
    )]
    async fn directory_get(
        &self,
        Parameters(args): Parameters<DirectoryGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let destination_path = match controller_destination_path(&args.destination_path) {
            Ok(path) => path,
            Err(error) => return Ok(error),
        };
        let chunk_size = match checked_transfer_chunk_size(args.chunk_size) {
            Ok(chunk_size) => chunk_size,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .directory_get(RemoteDirectoryGetRequest {
                device_id,
                device_root: args.target_root,
                destination_path,
                chunk_size,
            })
            .await
        {
            Ok(info) => directory_get_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Return Controller-owned durable progress for one directory transfer by TransferId."
    )]
    async fn directory_status(
        &self,
        Parameters(args): Parameters<TransferIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let transfer_id = match parse_transfer_id(&args.transfer_id) {
            Ok(id) => id,
            Err(error) => return Ok(error),
        };
        match self.client.directory_status(transfer_id).await {
            Ok(info) => directory_transfer_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Cancel one Controller-owned directory transfer by TransferId, including its in-flight child transfers."
    )]
    async fn directory_cancel(
        &self,
        Parameters(args): Parameters<TransferIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Full);
        let transfer_id = match parse_transfer_id(&args.transfer_id) {
            Ok(id) => id,
            Err(error) => return Ok(error),
        };
        match self.client.directory_cancel(transfer_id).await {
            Ok(info) => directory_transfer_result(info, verbosity),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Start a bounded batch of uploads from Controller A to target B. Each item chooses the existing single-file V5 plane or, with recursive=true, the existing recursive directory V5 plane. All Controller-A sources are preflighted before any transfer starts. Returns the durable child TransferIds; there is no transient batch-only transfer protocol."
    )]
    async fn transfer_put_batch(
        &self,
        Parameters(args): Parameters<TransferPutBatchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (device_id, items, verbosity) = match self.prepare_put_batch(&args).await {
            Ok(prepared) => prepared,
            Err(error) => return Ok(error),
        };
        let requested = items.len();
        let mut started = 0_usize;
        let mut results = Vec::with_capacity(requested);
        for (index, item) in items.into_iter().enumerate() {
            let result = match item {
                PreparedPutBatchItem::File {
                    source_path,
                    target_path,
                    chunk_size,
                    conflict,
                } => self
                    .client
                    .file_put(RemoteFilePutRequest {
                        device_id,
                        source_path,
                        device_path: target_path,
                        chunk_size,
                        conflict_policy: conflict.into(),
                    })
                    .await
                    .map(|info| file_put_value(&info, verbosity)),
                PreparedPutBatchItem::Directory {
                    source_path,
                    target_root,
                    chunk_size,
                } => self
                    .client
                    .directory_put(RemoteDirectoryPutRequest {
                        device_id,
                        source_path,
                        device_root: target_root,
                        chunk_size,
                    })
                    .await
                    .map(|info| directory_put_value(&info, verbosity)),
            };
            match result {
                Ok(result) => {
                    started += 1;
                    results.push(json!({ "index": index, "ok": true, "transfer": result }));
                }
                Err(error) => results.push(json!({
                    "index": index,
                    "ok": false,
                    "error": truncate_utf8(error.to_string(), MCP_ERROR_TEXT_BYTES),
                })),
            }
        }
        structured_result(json!({
            "requested": requested,
            "started": started,
            "failed_to_start": requested - started,
            "transfers": results,
        }))
    }

    #[tool(
        description = "Start a bounded batch of downloads from target B to Controller A. Each item uses the existing single-file V5 plane or, with recursive=true, the existing recursive directory V5 plane. All B-side source kinds and obvious Controller-A destination conflicts are preflighted before any transfer starts. Returns durable child TransferIds."
    )]
    async fn transfer_get_batch(
        &self,
        Parameters(args): Parameters<TransferGetBatchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (device_id, items, verbosity) = match self.prepare_get_batch(&args).await {
            Ok(prepared) => prepared,
            Err(error) => return Ok(error),
        };
        let requested = items.len();
        let mut started = 0_usize;
        let mut results = Vec::with_capacity(requested);
        for (index, item) in items.into_iter().enumerate() {
            let result = match item {
                PreparedGetBatchItem::File {
                    target_path,
                    destination_path,
                    chunk_size,
                    conflict,
                } => self
                    .client
                    .file_get(RemoteFileGetRequest {
                        device_id,
                        device_path: target_path,
                        destination_path,
                        chunk_size,
                        conflict_policy: conflict.into(),
                    })
                    .await
                    .map(|info| file_get_value(&info, verbosity)),
                PreparedGetBatchItem::Directory {
                    target_root,
                    destination_path,
                    chunk_size,
                } => self
                    .client
                    .directory_get(RemoteDirectoryGetRequest {
                        device_id,
                        device_root: target_root,
                        destination_path,
                        chunk_size,
                    })
                    .await
                    .map(|info| directory_get_value(&info, verbosity)),
            };
            match result {
                Ok(result) => {
                    started += 1;
                    results.push(json!({ "index": index, "ok": true, "transfer": result }));
                }
                Err(error) => results.push(json!({
                    "index": index,
                    "ok": false,
                    "error": truncate_utf8(error.to_string(), MCP_ERROR_TEXT_BYTES),
                })),
            }
        }
        structured_result(json!({
            "requested": requested,
            "started": started,
            "failed_to_start": requested - started,
            "transfers": results,
        }))
    }

    #[tool(
        description = "Query up to 32 durable file/directory TransferIds in one call. The caller supplies each transfer kind; Clew reuses the existing durable status stores. verbosity=minimal is suited to polling, summary adds progress, and full returns the underlying records."
    )]
    async fn transfer_batch_status(
        &self,
        Parameters(args): Parameters<TransferBatchStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transfers = match parse_transfer_references(&args.transfers) {
            Ok(transfers) => transfers,
            Err(error) => return Ok(error),
        };
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Summary);
        let requested = transfers.len();
        let mut terminal = 0_usize;
        let mut succeeded = 0_usize;
        let mut results = Vec::with_capacity(requested);
        for (index, (kind, transfer_id)) in transfers.into_iter().enumerate() {
            match kind {
                TransferReferenceKind::File => match self.client.file_status(transfer_id).await {
                    Ok(info) => {
                        succeeded += 1;
                        terminal += usize::from(info.phase().terminal());
                        results.push(json!({
                            "index": index,
                            "ok": true,
                            "transfer": file_transfer_value(&info, verbosity),
                        }));
                    }
                    Err(error) => results.push(json!({
                        "index": index,
                        "ok": false,
                        "kind": kind,
                        "transfer_id": transfer_id,
                        "error": truncate_utf8(error.to_string(), MCP_ERROR_TEXT_BYTES),
                    })),
                },
                TransferReferenceKind::Directory => {
                    match self.client.directory_status(transfer_id).await {
                        Ok(info) => {
                            succeeded += 1;
                            terminal += usize::from(directory_transfer_terminal(&info));
                            results.push(json!({
                                "index": index,
                                "ok": true,
                                "transfer": directory_transfer_value(&info, verbosity),
                            }));
                        }
                        Err(error) => results.push(json!({
                            "index": index,
                            "ok": false,
                            "kind": kind,
                            "transfer_id": transfer_id,
                            "error": truncate_utf8(error.to_string(), MCP_ERROR_TEXT_BYTES),
                        })),
                    }
                }
            }
        }
        structured_result(json!({
            "requested": requested,
            "queried": succeeded,
            "query_failures": requested - succeeded,
            "terminal": terminal,
            "active": succeeded - terminal,
            "transfers": results,
        }))
    }

    #[tool(
        description = "Cancel up to 32 durable file/directory transfers in one call. All kind/TransferId references are validated before the first cancellation. Cancellation delegates to the existing durable V5 file/directory cancel paths."
    )]
    async fn transfer_batch_cancel(
        &self,
        Parameters(args): Parameters<TransferBatchStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let transfers = match parse_transfer_references(&args.transfers) {
            Ok(transfers) => transfers,
            Err(error) => return Ok(error),
        };
        let verbosity = args.verbosity.unwrap_or(TransferVerbosity::Summary);
        let requested = transfers.len();
        let mut cancelled = 0_usize;
        let mut results = Vec::with_capacity(requested);
        for (index, (kind, transfer_id)) in transfers.into_iter().enumerate() {
            let result = match kind {
                TransferReferenceKind::File => self
                    .client
                    .file_cancel(transfer_id)
                    .await
                    .map(|info| file_transfer_value(&info, verbosity)),
                TransferReferenceKind::Directory => self
                    .client
                    .directory_cancel(transfer_id)
                    .await
                    .map(|info| directory_transfer_value(&info, verbosity)),
            };
            match result {
                Ok(result) => {
                    cancelled += 1;
                    results.push(json!({ "index": index, "ok": true, "transfer": result }));
                }
                Err(error) => results.push(json!({
                    "index": index,
                    "ok": false,
                    "kind": kind,
                    "transfer_id": transfer_id,
                    "error": truncate_utf8(error.to_string(), MCP_ERROR_TEXT_BYTES),
                })),
            }
        }
        structured_result(json!({
            "requested": requested,
            "cancel_requested": cancelled,
            "cancel_failures": requested - cancelled,
            "transfers": results,
        }))
    }

    #[tool(
        description = "Create exactly one directory on target B. The parent must already exist, the destination must not exist, and signed Write authority is required."
    )]
    async fn fs_mkdir(
        &self,
        Parameters(args): Parameters<FsPathControlArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::CreateDirectory { path: args.path },
        )
        .await
    }

    #[tool(
        description = "Copy one regular file on target B to a new path. This never overwrites and currently rejects directory copy; use directory_put/get for directory trees."
    )]
    async fn fs_copy(
        &self,
        Parameters(args): Parameters<FsCopyMoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::Copy {
                source: args.source,
                destination: args.destination,
            },
        )
        .await
    }

    #[tool(
        description = "Atomically move/rename a file or directory on target B to a new path. It never overwrites and intentionally fails rather than emulating a cross-filesystem move with copy+delete."
    )]
    async fn fs_move(
        &self,
        Parameters(args): Parameters<FsCopyMoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::Move {
                source: args.source,
                destination: args.destination,
            },
        )
        .await
    }

    #[tool(
        description = "Safely remove a file or directory from target B by moving it to the operating system Trash/Recycle Bin. There is deliberately no direct permanent-delete tool. Returns a Clew trash_id."
    )]
    async fn fs_trash(
        &self,
        Parameters(args): Parameters<FsPathControlArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::Trash { path: args.path },
        )
        .await
    }

    #[tool(
        description = "List only Trash/Recycle-Bin items that Clew itself placed there and is tracking for this target. It does not enumerate unrelated user trash items."
    )]
    async fn trash_list(
        &self,
        Parameters(args): Parameters<DeviceOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(args.device.as_deref(), FsMutationRequest::TrashList)
            .await
    }

    #[tool(
        description = "Restore one Clew-tracked Trash item to its original location. Fails on restore collision and is currently available where the OS Trash backend supports exact enumeration/restore."
    )]
    async fn trash_restore(
        &self,
        Parameters(args): Parameters<TrashIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::TrashRestore {
                trash_id: args.trash_id,
            },
        )
        .await
    }

    #[tool(
        description = "First half of permanent deletion. Verifies a Clew-tracked Trash item and returns a short-lived confirmation_token. This call does NOT permanently delete anything."
    )]
    async fn trash_purge_prepare(
        &self,
        Parameters(args): Parameters<TrashIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::TrashPurgePrepare {
                trash_id: args.trash_id,
            },
        )
        .await
    }

    #[tool(
        description = "Second half of permanent deletion. Permanently purges exactly one previously prepared Clew-tracked Trash item only when the fresh confirmation_token matches; wrong/expired tokens fail closed."
    )]
    async fn trash_purge_confirm(
        &self,
        Parameters(args): Parameters<TrashPurgeConfirmArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::TrashPurgeConfirm {
                trash_id: args.trash_id,
                confirmation_token: args.confirmation_token,
            },
        )
        .await
    }

    #[tool(
        description = "Create a Clew-owned temporary file or directory on target B. A purpose description is mandatory; Clew records stable resource_id, creation time and expiry for later cleanup instead of scattering orphan files."
    )]
    async fn temp_create(
        &self,
        Parameters(args): Parameters<TempCreateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let ttl_ms = match bounded_max_u64(
            args.ttl_ms.unwrap_or(MCP_DEFAULT_TEMP_TTL_MS),
            HARD_MAX_MANAGED_TEMP_TTL_MS,
            "temp ttl_ms",
        ) {
            Ok(ttl_ms) => ttl_ms,
            Err(error) => return Ok(error),
        };
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::TempCreate {
                temp_kind: args.kind.into(),
                description: args.description,
                ttl_ms,
            },
        )
        .await
    }

    #[tool(
        description = "List Clew-managed temporary resources on target B with resource_id, kind, path, purpose and expiry."
    )]
    async fn temp_list(
        &self,
        Parameters(args): Parameters<DeviceOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(args.device.as_deref(), FsMutationRequest::TempList)
            .await
    }

    #[tool(
        description = "Release one Clew-managed temporary resource immediately. Cleanup is confined to Clew's managed namespace and cannot target arbitrary user paths."
    )]
    async fn temp_release(
        &self,
        Parameters(args): Parameters<TempIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(
            args.device.as_deref(),
            FsMutationRequest::TempRelease {
                resource_id: args.resource_id,
            },
        )
        .await
    }

    #[tool(
        description = "Reclaim expired Clew-managed temporary resources on target B. GC only removes resources registered in Clew's own managed ledger; it never scans/deletes arbitrary temp directories."
    )]
    async fn temp_gc(
        &self,
        Parameters(args): Parameters<DeviceOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.execute_fs_control(args.device.as_deref(), FsMutationRequest::TempGc)
            .await
    }

    #[tool(
        description = "Start one bounded live-session Shell task. Requires explicit signed Shell authority. The cwd is only the initial directory, not a filesystem sandbox."
    )]
    async fn shell_start(
        &self,
        Parameters(args): Parameters<ShellStartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device_id = match self.resolve_device(args.device.as_deref()).await {
            Ok(device_id) => device_id,
            Err(error) => return Ok(error),
        };
        let timeout_ms = match bounded_max_u32(
            args.timeout_ms.unwrap_or(MCP_DEFAULT_SHELL_TIMEOUT_MS),
            HARD_MAX_SHELL_TIMEOUT_MS,
            "shell timeout_ms",
        ) {
            Ok(timeout_ms) => timeout_ms,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .shell_start(RemoteShellStartRequest {
                device_id,
                command: args.command,
                cwd: args.cwd,
                env: args.env,
                timeout_ms,
            })
            .await
        {
            Ok(task_id) => structured_result(json!({ "task_id": task_id })),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Get the current state of a Controller-projected Shell TaskId. During V3's bounded reconnect grace, the Controller may re-prove the same TaskId on a newly authenticated session for the same DeviceId."
    )]
    async fn shell_status(
        &self,
        Parameters(args): Parameters<ShellTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let task_id = match parse_task_id(&args.task_id) {
            Ok(task_id) => task_id,
            Err(error) => return Ok(error),
        };
        match self.client.shell_status(task_id).await {
            Ok(status) => structured_result(status),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Fetch bounded stdout/stderr chunks for a Shell TaskId using absolute byte cursors. A confirmed task may reattach only to the same authenticated DeviceId during V3's bounded reconnect grace; output remains hard-bounded."
    )]
    async fn shell_attach(
        &self,
        Parameters(args): Parameters<ShellAttachArgs>,
    ) -> Result<CallToolResult, McpError> {
        let task_id = match parse_task_id(&args.task_id) {
            Ok(task_id) => task_id,
            Err(error) => return Ok(error),
        };
        let max_bytes_per_stream = match bounded_max_u32(
            args.max_bytes_per_stream
                .unwrap_or(MCP_DEFAULT_SHELL_ATTACH_BYTES),
            HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM,
            "shell max_bytes_per_stream",
        ) {
            Ok(max_bytes_per_stream) => max_bytes_per_stream,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .shell_attach(RemoteShellAttachRequest {
                task_id,
                stdout_offset: args.stdout_offset.unwrap_or(0),
                stderr_offset: args.stderr_offset.unwrap_or(0),
                max_bytes_per_stream,
            })
            .await
        {
            Ok(output) => structured_result(output),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Request cancellation of a Shell TaskId. The Controller resolves the task to its original DeviceId and may re-prove it after a bounded reconnect; callers cannot supply another device."
    )]
    async fn shell_cancel(
        &self,
        Parameters(args): Parameters<ShellTaskArgs>,
    ) -> Result<CallToolResult, McpError> {
        let task_id = match parse_task_id(&args.task_id) {
            Ok(task_id) => task_id,
            Err(error) => return Ok(error),
        };
        match self.client.shell_cancel(task_id).await {
            Ok(()) => structured_result(json!({ "task_id": task_id, "cancelled": true })),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }
}

#[tool_handler(
    name = "clew",
    instructions = "Clew exposes bounded tools through Controller A. List devices first when selection is ambiguous; helper-only C is never executable. Target-B filesystem paths accept absolute paths or ~/... for the OS account running Clew; ~otheruser is intentionally unsupported and all expanded paths still pass signed scope, OS ACL, canonicalization, and symlink/reparse checks. Read/query/file Get require signed Read authority; Write/Edit/file Put and controlled filesystem mutations require signed Write authority. File transfer uses the durable V5 file/directory planes: directory operations are recursive, and transfer_*_batch multiplexes bounded file plus recursive-directory items without inventing transient transfer state. Use minimal/summary/full verbosity to control polling output and retain the durable child TransferIds. Shell requires explicit signed Shell authority and is not the preferred way to delete files."
)]
impl ServerHandler for ClewMcpServer {}

pub async fn serve_stdio(config: ControllerConfig) -> Result<(), Box<dyn Error>> {
    ensure_controller(&config).await?;
    let service = ClewMcpServer::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub async fn serve_http(
    config: ControllerConfig,
    listen: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    validate_http_listen(listen)?;
    ensure_controller(&config).await?;

    let listener = tokio::net::TcpListener::bind(listen).await?;
    let actual = listener.local_addr()?;
    let server = ClewMcpServer::new(config);
    let http_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_origins(http_allowed_origins(actual))
        .with_max_request_body_bytes(MCP_HTTP_MAX_REQUEST_BODY_BYTES);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    println!("Clew MCP Streamable HTTP ready at http://{actual}/mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn validate_http_listen(listen: SocketAddr) -> Result<(), Box<dyn Error>> {
    if !listen.ip().is_loopback() {
        return Err(
            format!("Clew V2 MCP HTTP requires a loopback listener; refused {listen}").into(),
        );
    }
    Ok(())
}

fn http_allowed_origins(listen: SocketAddr) -> Vec<String> {
    let port = listen.port();
    let direct = match listen.ip() {
        std::net::IpAddr::V4(ip) => format!("http://{ip}:{port}"),
        std::net::IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
    };
    vec![direct, format!("http://localhost:{port}")]
}

async fn ensure_controller(config: &ControllerConfig) -> Result<(), Box<dyn Error>> {
    let client = LocalApiClient::new(config.clone());
    if client.controller_status().await.is_ok() {
        return Ok(());
    }

    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("controller")
        .arg("--state-dir")
        .arg(config.state_root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let deadline = tokio::time::Instant::now() + MCP_CONTROLLER_START_TIMEOUT;
    loop {
        if client.controller_status().await.is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("Clew Controller exited before MCP became ready: {status}").into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Clew Controller did not become ready for MCP within 8 seconds".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn structured_result(value: impl serde::Serialize) -> Result<CallToolResult, McpError> {
    let value = serde_json::to_value(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::structured(value))
}

fn validate_transfer_batch_len(len: usize, max: usize) -> Result<(), CallToolResult> {
    if len == 0 || len > max {
        return Err(tool_error(format!(
            "transfer batch must contain 1..={max} items, got {len}"
        )));
    }
    Ok(())
}

fn phase_value<T: Serialize>(phase: T) -> serde_json::Value {
    serde_json::to_value(phase).unwrap_or_else(|_| json!("unknown"))
}

fn file_put_value(info: &FilePutInfo, verbosity: TransferVerbosity) -> serde_json::Value {
    match verbosity {
        TransferVerbosity::Minimal => json!({
            "kind": "file",
            "direction": "put",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
        }),
        TransferVerbosity::Summary => json!({
            "kind": "file",
            "direction": "put",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
            "source_path": info.source_path,
            "target_path": info.device_path,
            "confirmed_bytes": info.confirmed_offset,
            "total_bytes": info.total_size,
            "final_target_path": info.final_device_path,
            "error": info.error,
        }),
        TransferVerbosity::Full => serde_json::to_value(info).unwrap_or_else(|_| {
            json!({
                "kind": "file",
                "direction": "put",
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            })
        }),
    }
}

fn file_get_value(info: &FileGetInfo, verbosity: TransferVerbosity) -> serde_json::Value {
    match verbosity {
        TransferVerbosity::Minimal => json!({
            "kind": "file",
            "direction": "get",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
        }),
        TransferVerbosity::Summary => json!({
            "kind": "file",
            "direction": "get",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
            "target_path": info.device_path,
            "destination_path": info.destination_path,
            "confirmed_bytes": info.confirmed_offset,
            "total_bytes": info.total_size,
            "final_destination_path": info.final_controller_path,
            "error": info.error,
        }),
        TransferVerbosity::Full => serde_json::to_value(info).unwrap_or_else(|_| {
            json!({
                "kind": "file",
                "direction": "get",
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            })
        }),
    }
}

fn file_transfer_value(info: &FileTransferInfo, verbosity: TransferVerbosity) -> serde_json::Value {
    if matches!(verbosity, TransferVerbosity::Full) {
        return serde_json::to_value(info).unwrap_or_else(|_| {
            json!({
                "transfer_id": info.transfer_id(),
                "phase": phase_value(info.phase()),
            })
        });
    }
    match info {
        FileTransferInfo::Put(info) => file_put_value(info, verbosity),
        FileTransferInfo::Get(info) => file_get_value(info, verbosity),
    }
}

fn file_put_result(
    info: FilePutInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(file_put_value(&info, verbosity))
}

fn file_get_result(
    info: FileGetInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(file_get_value(&info, verbosity))
}

fn file_transfer_result(
    info: FileTransferInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(file_transfer_value(&info, verbosity))
}

fn directory_put_value(info: &DirectoryPutInfo, verbosity: TransferVerbosity) -> serde_json::Value {
    match verbosity {
        TransferVerbosity::Minimal => json!({
            "kind": "directory",
            "direction": "put",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
        }),
        TransferVerbosity::Summary => json!({
            "kind": "directory",
            "direction": "put",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
            "source_path": info.source_path,
            "target_root": info.device_root,
            "confirmed_bytes": info.confirmed_file_bytes,
            "total_bytes": info.total_file_bytes,
            "completed_files": info.completed_files,
            "total_files": info.total_files,
            "current_relative_path": info.current_relative_path,
            "final_target_root": info.final_device_root,
            "error": info.error,
        }),
        TransferVerbosity::Full => serde_json::to_value(info).unwrap_or_else(|_| {
            json!({
                "kind": "directory",
                "direction": "put",
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            })
        }),
    }
}

fn directory_get_value(info: &DirectoryGetInfo, verbosity: TransferVerbosity) -> serde_json::Value {
    match verbosity {
        TransferVerbosity::Minimal => json!({
            "kind": "directory",
            "direction": "get",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
        }),
        TransferVerbosity::Summary => json!({
            "kind": "directory",
            "direction": "get",
            "transfer_id": info.transfer_id,
            "phase": phase_value(info.phase),
            "target_root": info.device_root,
            "destination_path": info.destination_path,
            "confirmed_bytes": info.confirmed_file_bytes,
            "total_bytes": info.total_file_bytes,
            "completed_files": info.completed_files,
            "total_files": info.total_files,
            "current_relative_path": info.current_relative_path,
            "final_destination_path": info.final_destination_path,
            "error": info.error,
        }),
        TransferVerbosity::Full => serde_json::to_value(info).unwrap_or_else(|_| {
            json!({
                "kind": "directory",
                "direction": "get",
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            })
        }),
    }
}

fn directory_transfer_value(
    info: &DirectoryTransferInfo,
    verbosity: TransferVerbosity,
) -> serde_json::Value {
    if matches!(verbosity, TransferVerbosity::Full) {
        return serde_json::to_value(info).unwrap_or_else(|_| match info {
            DirectoryTransferInfo::Put(info) => json!({
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            }),
            DirectoryTransferInfo::Get(info) => json!({
                "transfer_id": info.transfer_id,
                "phase": phase_value(info.phase),
            }),
        });
    }
    match info {
        DirectoryTransferInfo::Put(info) => directory_put_value(info, verbosity),
        DirectoryTransferInfo::Get(info) => directory_get_value(info, verbosity),
    }
}

fn directory_put_result(
    info: DirectoryPutInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(directory_put_value(&info, verbosity))
}

fn directory_get_result(
    info: DirectoryGetInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(directory_get_value(&info, verbosity))
}

fn directory_transfer_result(
    info: DirectoryTransferInfo,
    verbosity: TransferVerbosity,
) -> Result<CallToolResult, McpError> {
    structured_result(directory_transfer_value(&info, verbosity))
}

fn directory_transfer_terminal(info: &DirectoryTransferInfo) -> bool {
    match info {
        DirectoryTransferInfo::Put(info) => info.phase.terminal(),
        DirectoryTransferInfo::Get(info) => info.phase.terminal(),
    }
}

fn parse_transfer_references(
    refs: &[TransferReferenceArg],
) -> Result<Vec<(TransferReferenceKind, TransferId)>, CallToolResult> {
    validate_transfer_batch_len(refs.len(), MCP_MAX_TRANSFER_STATUS_ITEMS)?;
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(refs.len());
    for (index, reference) in refs.iter().enumerate() {
        let transfer_id = reference.transfer_id.parse::<TransferId>().map_err(|_| {
            tool_error(format!(
                "transfer reference {index} has an invalid canonical non-nil TransferId"
            ))
        })?;
        let key = (
            match reference.kind {
                TransferReferenceKind::File => 0_u8,
                TransferReferenceKind::Directory => 1_u8,
            },
            transfer_id,
        );
        if !seen.insert(key) {
            return Err(tool_error(format!(
                "transfer reference {index} duplicates an earlier kind/TransferId pair"
            )));
        }
        parsed.push((reference.kind, transfer_id));
    }
    Ok(parsed)
}

fn parse_task_id(value: &str) -> Result<TaskId, CallToolResult> {
    value
        .parse::<TaskId>()
        .map_err(|_| tool_error("task_id must be a canonical non-nil Clew TaskId"))
}

fn parse_transfer_id(value: &str) -> Result<TransferId, CallToolResult> {
    value
        .parse::<TransferId>()
        .map_err(|_| tool_error("transfer_id must be a canonical non-nil Clew TransferId"))
}

fn controller_destination_path(value: &str) -> Result<String, CallToolResult> {
    let path = std::path::PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(tool_error_from_display)?
            .join(path)
    };
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| tool_error("Controller-A destination path must be valid UTF-8"))
}

fn tool_error_from_display(error: impl std::fmt::Display) -> CallToolResult {
    tool_error(error.to_string())
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    let message = truncate_utf8(message.into(), MCP_ERROR_TEXT_BYTES);
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn bounded_max_u32(value: u32, max: u32, label: &str) -> Result<u32, CallToolResult> {
    if value == 0 {
        return Err(tool_error(format!("{label} must be greater than zero")));
    }
    Ok(value.min(max))
}

fn bounded_max_u64(value: u64, max: u64, label: &str) -> Result<u64, CallToolResult> {
    if value == 0 {
        return Err(tool_error(format!("{label} must be greater than zero")));
    }
    Ok(value.min(max))
}

fn checked_transfer_chunk_size(value: Option<u32>) -> Result<u32, CallToolResult> {
    let value = value.unwrap_or(MCP_DEFAULT_FILE_CHUNK_BYTES);
    if value < MIN_FILE_CHUNK_BYTES || value > MAX_FILE_CHUNK_BYTES || !value.is_power_of_two() {
        return Err(tool_error(format!(
            "chunk_size must be a power of two within {MIN_FILE_CHUNK_BYTES}..={MAX_FILE_CHUNK_BYTES} bytes"
        )));
    }
    Ok(value)
}

fn read_output_value(
    device_id: DeviceId,
    path: &str,
    offset: u64,
    data: Vec<u8>,
    output: ReadOutput,
) -> Result<serde_json::Value, &'static str> {
    let bytes = data.len();
    match output {
        ReadOutput::Base64 => Ok(json!({
            "device_id": device_id,
            "path": path,
            "offset": offset,
            "bytes": bytes,
            "encoding": "base64",
            "data_base64": BASE64_STANDARD.encode(data),
        })),
        ReadOutput::Text => match String::from_utf8(data) {
            Ok(text) => Ok(json!({
                "device_id": device_id,
                "path": path,
                "offset": offset,
                "bytes": bytes,
                "encoding": "utf-8",
                "text": text,
            })),
            Err(_) => Err(
                "read output=text requires valid UTF-8; retry with output=base64 or output=auto",
            ),
        },
        ReadOutput::Auto => match String::from_utf8(data) {
            Ok(text) => Ok(json!({
                "device_id": device_id,
                "path": path,
                "offset": offset,
                "bytes": bytes,
                "encoding": "utf-8",
                "text": text,
            })),
            Err(error) => Ok(json!({
                "device_id": device_id,
                "path": path,
                "offset": offset,
                "bytes": bytes,
                "encoding": "base64",
                "data_base64": BASE64_STANDARD.encode(error.into_bytes()),
            })),
        },
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_output_auto_prefers_utf8_and_falls_back_to_base64() {
        let device_id = DeviceId::new();
        let text = read_output_value(
            device_id,
            "~/.ssh/config",
            0,
            "Host mzd\n  HostName 10.0.0.2\n".as_bytes().to_vec(),
            ReadOutput::Auto,
        )
        .unwrap();
        assert_eq!(text["encoding"], "utf-8");
        assert_eq!(text["text"], "Host mzd\n  HostName 10.0.0.2\n");
        assert!(text.get("data_base64").is_none());

        let binary = read_output_value(
            device_id,
            "~/binary.dat",
            7,
            vec![0xff, 0x00, 0x80],
            ReadOutput::Auto,
        )
        .unwrap();
        assert_eq!(binary["encoding"], "base64");
        assert_eq!(
            binary["data_base64"],
            BASE64_STANDARD.encode([0xff, 0x00, 0x80])
        );
        assert!(binary.get("text").is_none());
    }

    #[test]
    fn read_output_text_rejects_invalid_utf8_and_base64_is_explicit() {
        let device_id = DeviceId::new();
        assert!(
            read_output_value(device_id, "~/binary.dat", 0, vec![0xff], ReadOutput::Text,).is_err()
        );
        let base64 = read_output_value(
            device_id,
            "~/.ssh/config",
            0,
            b"Host mzd".to_vec(),
            ReadOutput::Base64,
        )
        .unwrap();
        assert_eq!(base64["encoding"], "base64");
        assert_eq!(base64["data_base64"], BASE64_STANDARD.encode(b"Host mzd"));
    }

    #[test]
    fn streamable_http_listen_is_loopback_only() {
        assert!(validate_http_listen("127.0.0.1:0".parse().unwrap()).is_ok());
        assert!(validate_http_listen("[::1]:0".parse().unwrap()).is_ok());
        assert!(validate_http_listen("0.0.0.0:4877".parse().unwrap()).is_err());
        assert!(validate_http_listen("192.0.2.1:4877".parse().unwrap()).is_err());
    }

    #[test]
    fn streamable_http_origin_allowlist_is_exact_to_the_bound_loopback_port() {
        assert_eq!(
            http_allowed_origins("127.0.0.1:4877".parse().unwrap()),
            vec![
                "http://127.0.0.1:4877".to_owned(),
                "http://localhost:4877".to_owned(),
            ]
        );
        assert_eq!(
            http_allowed_origins("[::1]:4878".parse().unwrap()),
            vec![
                "http://[::1]:4878".to_owned(),
                "http://localhost:4878".to_owned(),
            ]
        );
    }

    #[test]
    fn mcp_error_text_is_utf8_safe_and_bounded() {
        let message = "界".repeat(MCP_ERROR_TEXT_BYTES);
        let truncated = truncate_utf8(message, MCP_ERROR_TEXT_BYTES);
        assert!(truncated.len() <= MCP_ERROR_TEXT_BYTES);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn user_maximum_hints_are_safely_capped_without_weakening_protocol_bounds() {
        assert_eq!(
            bounded_max_u32(65_536, HARD_MAX_READ_RESULT_BYTES, "read").unwrap(),
            HARD_MAX_READ_RESULT_BYTES
        );
        assert_eq!(
            bounded_max_u32(u32::MAX, HARD_MAX_FS_RESULT_ITEMS, "items").unwrap(),
            HARD_MAX_FS_RESULT_ITEMS
        );
        assert_eq!(
            bounded_max_u64(u64::MAX, HARD_MAX_GREP_SCAN_BYTES, "scan").unwrap(),
            HARD_MAX_GREP_SCAN_BYTES
        );
        assert_eq!(
            bounded_max_u64(u64::MAX, HARD_MAX_MANAGED_TEMP_TTL_MS, "ttl").unwrap(),
            HARD_MAX_MANAGED_TEMP_TTL_MS
        );
        assert!(bounded_max_u32(0, HARD_MAX_READ_RESULT_BYTES, "read").is_err());
        assert!(bounded_max_u64(0, HARD_MAX_GREP_SCAN_BYTES, "scan").is_err());
    }

    #[test]
    fn custom_transfer_chunk_sizes_fail_early_and_defaults_stay_protocol_valid() {
        assert_eq!(
            checked_transfer_chunk_size(None).unwrap(),
            MCP_DEFAULT_FILE_CHUNK_BYTES
        );
        assert_eq!(
            checked_transfer_chunk_size(Some(MAX_FILE_CHUNK_BYTES)).unwrap(),
            MAX_FILE_CHUNK_BYTES
        );
        assert!(checked_transfer_chunk_size(Some(0)).is_err());
        assert!(checked_transfer_chunk_size(Some(MIN_FILE_CHUNK_BYTES + 1)).is_err());
        assert!(checked_transfer_chunk_size(Some(MAX_FILE_CHUNK_BYTES + 1)).is_err());
    }

    #[test]
    fn transfer_batch_bounds_are_explicit_and_bounded() {
        assert!(validate_transfer_batch_len(1, MCP_MAX_TRANSFER_BATCH_ITEMS).is_ok());
        assert!(
            validate_transfer_batch_len(MCP_MAX_TRANSFER_BATCH_ITEMS, MCP_MAX_TRANSFER_BATCH_ITEMS)
                .is_ok()
        );
        assert!(validate_transfer_batch_len(0, MCP_MAX_TRANSFER_BATCH_ITEMS).is_err());
        assert!(
            validate_transfer_batch_len(
                MCP_MAX_TRANSFER_BATCH_ITEMS + 1,
                MCP_MAX_TRANSFER_BATCH_ITEMS,
            )
            .is_err()
        );
        assert!(
            validate_transfer_batch_len(
                MCP_MAX_TRANSFER_STATUS_ITEMS,
                MCP_MAX_TRANSFER_STATUS_ITEMS,
            )
            .is_ok()
        );
    }

    #[test]
    fn transfer_verbosity_full_preserves_existing_status_shape() {
        let info = FileTransferInfo::Put(FilePutInfo {
            transfer_id: TransferId::new(),
            device_id: DeviceId::new(),
            source_path: "C:/controller/source.bin".into(),
            device_path: "C:/target/destination.bin".into(),
            phase: clew_runtime::ControllerFileTransferPhase::Running,
            chunk_size: MCP_DEFAULT_FILE_CHUNK_BYTES,
            total_size: Some(1234),
            final_sha256: None,
            confirmed_offset: 321,
            final_device_path: None,
            error: None,
        });
        assert_eq!(
            file_transfer_value(&info, TransferVerbosity::Full),
            serde_json::to_value(&info).unwrap()
        );
        let minimal = file_transfer_value(&info, TransferVerbosity::Minimal);
        assert_eq!(minimal["kind"], "file");
        assert_eq!(minimal["direction"], "put");
        assert!(minimal.get("source_path").is_none());
        let summary = file_transfer_value(&info, TransferVerbosity::Summary);
        assert_eq!(summary["confirmed_bytes"], 321);
        assert_eq!(summary["total_bytes"], 1234);
        assert!(summary.get("source_path").is_some());
    }

    #[test]
    fn tool_router_exposes_v8_filesystem_surface_without_removing_existing_tools() {
        let mut names: Vec<_> = ClewMcpServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "devices",
                "directory_cancel",
                "directory_get",
                "directory_put",
                "directory_status",
                "edit",
                "file_cancel",
                "file_get",
                "file_put",
                "file_status",
                "fs_copy",
                "fs_mkdir",
                "fs_move",
                "fs_trash",
                "glob",
                "grep",
                "path_info",
                "read",
                "session_path_info",
                "shell_attach",
                "shell_cancel",
                "shell_start",
                "shell_status",
                "temp_create",
                "temp_gc",
                "temp_list",
                "temp_release",
                "transfer_batch_cancel",
                "transfer_batch_status",
                "transfer_get_batch",
                "transfer_put_batch",
                "trash_list",
                "trash_purge_confirm",
                "trash_purge_prepare",
                "trash_restore",
                "write",
            ]
        );
    }
}
