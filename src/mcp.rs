use std::{
    collections::BTreeMap,
    error::Error,
    process::{Command, Stdio},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_core::{DeviceId, TaskId, select_executable_device};
use clew_runtime::{
    ControllerConfig, FsWritePrecondition, LocalApiClient, RemoteEditRequest, RemoteGlobRequest,
    RemoteGrepRequest, RemotePathInfoRequest, RemoteReadRequest, RemoteShellAttachRequest,
    RemoteShellStartRequest, RemoteWriteRequest,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars::{self, JsonSchema},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use serde_json::json;

const MCP_DEFAULT_READ_LIMIT: u32 = 16_384;
const MCP_DEFAULT_PAGE_LIMIT: u32 = 128;
const MCP_DEFAULT_RESULT_BYTES: u32 = 32_768;
const MCP_DEFAULT_GREP_SCAN_BYTES: u64 = 8_388_608;
const MCP_DEFAULT_SHELL_TIMEOUT_MS: u32 = 300_000;
const MCP_DEFAULT_SHELL_ATTACH_BYTES: u32 = 12_288;
const MCP_ERROR_TEXT_BYTES: usize = 2_048;
const MCP_CONTROLLER_START_TIMEOUT: Duration = Duration::from_secs(8);

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
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadArgs {
    #[serde(default)]
    #[schemars(
        description = "Optional DeviceId, Site/Device, or unique executable short name. Omit only when exactly one online executable device exists."
    )]
    device: Option<String>,
    #[schemars(description = "Absolute path allowed by the signed Site read policy.")]
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    #[schemars(
        description = "Maximum raw bytes to return. Defaults to 16384 and remains subject to Clew's signed and protocol hard bounds."
    )]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PathInfoArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(description = "Absolute path allowed by the signed Site read policy.")]
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GlobArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(description = "Absolute signed root or subdirectory to search.")]
    root: String,
    #[schemars(description = "Relative bounded glob pattern, for example **/*.rs.")]
    pattern: String,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    max_bytes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrepArgs {
    #[serde(default)]
    device: Option<String>,
    #[schemars(description = "Absolute signed root or file to search.")]
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
    limit: Option<u32>,
    #[serde(default)]
    max_bytes: Option<u32>,
    #[serde(default)]
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
        description = "Absolute initial working directory inside a signed root. This is not a filesystem sandbox."
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
    max_bytes_per_stream: Option<u32>,
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
        description = "Read one bounded byte range from an executable device. Returns base64 so binary data is preserved. Do not scan whole disks; use the signed roots and small ranges."
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
        let limit = args.limit.unwrap_or(MCP_DEFAULT_READ_LIMIT);
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
            Ok(result) => structured_result(json!({
                "device_id": device_id,
                "path": args.path,
                "offset": offset,
                "bytes": result.data.len(),
                "data_base64": BASE64_STANDARD.encode(result.data),
            })),
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
        match self
            .client
            .glob(RemoteGlobRequest {
                device_id,
                root: args.root,
                pattern: args.pattern,
                cursor: args.cursor.unwrap_or(0),
                limit: args.limit.unwrap_or(MCP_DEFAULT_PAGE_LIMIT),
                max_bytes: args.max_bytes.unwrap_or(MCP_DEFAULT_RESULT_BYTES),
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
        match self
            .client
            .grep(RemoteGrepRequest {
                device_id,
                root: args.root,
                pattern: args.pattern,
                include: args.include,
                cursor: args.cursor.unwrap_or(0),
                limit: args.limit.unwrap_or(MCP_DEFAULT_PAGE_LIMIT),
                max_bytes: args.max_bytes.unwrap_or(MCP_DEFAULT_RESULT_BYTES),
                max_scan_bytes: args.max_scan_bytes.unwrap_or(MCP_DEFAULT_GREP_SCAN_BYTES),
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
        match self
            .client
            .shell_start(RemoteShellStartRequest {
                device_id,
                command: args.command,
                cwd: args.cwd,
                env: args.env,
                timeout_ms: args.timeout_ms.unwrap_or(MCP_DEFAULT_SHELL_TIMEOUT_MS),
            })
            .await
        {
            Ok(task_id) => structured_result(json!({ "task_id": task_id })),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Get the current state of a Shell TaskId in this live Controller/Target session. Reconnect reattach is intentionally not a V2 capability."
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
        description = "Fetch bounded stdout/stderr chunks for a live Shell TaskId using absolute byte cursors. Output chunks remain hard-bounded by Clew."
    )]
    async fn shell_attach(
        &self,
        Parameters(args): Parameters<ShellAttachArgs>,
    ) -> Result<CallToolResult, McpError> {
        let task_id = match parse_task_id(&args.task_id) {
            Ok(task_id) => task_id,
            Err(error) => return Ok(error),
        };
        match self
            .client
            .shell_attach(RemoteShellAttachRequest {
                task_id,
                stdout_offset: args.stdout_offset.unwrap_or(0),
                stderr_offset: args.stderr_offset.unwrap_or(0),
                max_bytes_per_stream: args
                    .max_bytes_per_stream
                    .unwrap_or(MCP_DEFAULT_SHELL_ATTACH_BYTES),
            })
            .await
        {
            Ok(output) => structured_result(output),
            Err(error) => Ok(tool_error_from_display(error)),
        }
    }

    #[tool(
        description = "Request cancellation of a live Shell TaskId. The Controller resolves the task to its original live device session; callers cannot supply another device."
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
    instructions = "Clew exposes bounded tools through the local Controller. List devices first when selection is ambiguous. Helper-only devices are never executable. Prefer narrow signed roots and bounded reads/searches. Write/Edit/Shell require explicit signed authority. Shell TaskIds are valid only in the current live device session."
)]
impl ServerHandler for ClewMcpServer {}

pub async fn serve_stdio(config: ControllerConfig) -> Result<(), Box<dyn Error>> {
    ensure_controller(&config).await?;
    let service = ClewMcpServer::new(config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn ensure_controller(config: &ControllerConfig) -> Result<(), Box<dyn Error>> {
    let client = LocalApiClient::new(config.clone());
    if client.controller_status().await.is_ok() {
        return Ok(());
    }

    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("controller")
        .arg("--state-dir")
        .arg(config.state_root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
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

fn parse_task_id(value: &str) -> Result<TaskId, CallToolResult> {
    value
        .parse::<TaskId>()
        .map_err(|_| tool_error("task_id must be a canonical non-nil Clew TaskId"))
}

fn tool_error_from_display(error: impl std::fmt::Display) -> CallToolResult {
    tool_error(error.to_string())
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    let message = truncate_utf8(message.into(), MCP_ERROR_TEXT_BYTES);
    CallToolResult::error(vec![ContentBlock::text(message)])
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
    fn mcp_error_text_is_utf8_safe_and_bounded() {
        let message = "界".repeat(MCP_ERROR_TEXT_BYTES);
        let truncated = truncate_utf8(message, MCP_ERROR_TEXT_BYTES);
        assert!(truncated.len() <= MCP_ERROR_TEXT_BYTES);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn tool_router_exposes_exact_v2_agent_surface() {
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
                "edit",
                "glob",
                "grep",
                "path_info",
                "read",
                "shell_attach",
                "shell_cancel",
                "shell_start",
                "shell_status",
                "write",
            ]
        );
    }
}
