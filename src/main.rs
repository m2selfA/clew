use std::{collections::BTreeMap, io::Write, net::SocketAddr, path::PathBuf, process::ExitCode};

#[cfg(any(windows, target_os = "macos"))]
mod gui;
#[cfg(any(windows, target_os = "macos"))]
mod host_gui;
mod invite_io;
mod mcp;
#[cfg(any(windows, target_os = "macos"))]
mod studio;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clew_core::{
    DeviceId, ForwardId, InviteId, ProxyId, TaskId, TransferId, select_executable_device,
};
use clew_host::{
    HostInstanceStart, HostLaunchContext, HostLaunchMode, HostLaunchState, OutfitPreset,
    acquire_host_instance, resolve_host_launch_with_mode,
    serve_networked_membership_until_with_layout, wait_for_networked_activation_until,
};
#[cfg(any(windows, target_os = "macos"))]
use clew_host::{HostMembershipStore, HostSiteSource};
use clew_runtime::{
    BackupExportRequest, ControllerConfig, ControllerStart, FileConflictPolicy, ForwardAddRequest,
    FsWritePrecondition, HttpConnectAddRequest, InviteIssueRequest, LocalApiClient,
    OutfitAssetImportRequest, OutfitCloneRequest, OutfitCreateRequest, OutfitSetAssetRequest,
    OutfitSetFieldRequest, RemoteDirectoryPutRequest, RemoteEditRequest, RemoteFileGetRequest,
    RemoteFilePutRequest, RemoteGlobRequest, RemoteGrepRequest, RemotePathInfoRequest,
    RemoteReadRequest, RemoteShellAttachRequest, RemoteShellStartRequest, RemoteWriteRequest,
    Socks5AddRequest, restore_controller_backup, start_controller,
};

#[derive(Debug, Parser)]
#[command(
    name = "clew",
    version,
    about = "Agent-facing remote capability bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct MintArgs {
    site_name: String,
    #[arg(long, value_name = "OUTFIT_ID")]
    outfit: Option<String>,
    #[arg(long = "root", value_name = "DIR", required = true)]
    roots: Vec<PathBuf>,
    #[arg(long, value_name = "FILE", default_value = "site.clew")]
    output: PathBuf,
    #[arg(long, default_value_t = 8)]
    max_claims: u32,
    #[arg(long, default_value_t = 168)]
    valid_hours: u64,
    #[arg(long, default_value_t = 24)]
    deployment_hours: u64,
    #[arg(long, default_value_t = 49_152)]
    max_result_bytes: u32,
    #[arg(long, default_value_t = 5_000)]
    read_timeout_ms: u32,
    /// Explicitly grant bounded V2 Edit/Write authority inside the signed roots.
    #[arg(long)]
    allow_write: bool,
    /// Explicitly grant V2 Shell task authority. Disabled by default.
    #[arg(long)]
    allow_shell: bool,
    /// Explicitly grant V4 TCP egress authority. Disabled by default.
    #[arg(long)]
    allow_tcp_egress: bool,
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the collaborator Host. Desktop platforms show a window/tray; Linux stays foreground.
    Host {
        /// Explicit invitation sidecar. Otherwise Clew uses the fixed sibling/state recovery order.
        #[arg(long, value_name = "FILE")]
        site: Option<PathBuf>,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
        /// Keep status in the terminal instead of opening the desktop Host window.
        #[arg(long)]
        foreground: bool,
        /// Use this Site Kit only as a nearby connection helper. This can only reduce authority.
        #[arg(long)]
        connector_only: bool,
    },
    /// Run the persistent local Controller, or attach to the existing owner.
    Controller {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Open the Controller GUI. Starts the Controller automatically if needed.
    Gui {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    #[command(alias = "invite")]
    /// Create a signed networked site.clew invitation for this Controller platform.
    Mint(MintArgs),
    /// Manage reusable Distribution Studio Outfit profiles through the Controller Local API.
    Outfit {
        #[command(subcommand)]
        command: OutfitCommand,
    },
    /// Close one bootstrap invite to future claims while keeping enrolled devices.
    InviteClose {
        invite_id: InviteId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Read one bounded byte range from an enrolled executable device.
    ///
    /// With two operands, the first is a DeviceId, Site/Device qualified name, or unique short
    /// name and the second is the path. With one operand, it is the path and Clew selects the
    /// device only when exactly one online executable device exists.
    Read {
        #[arg(value_name = "DEVICE_OR_PATH", num_args = 1..=2)]
        operands: Vec<String>,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 16_384)]
        limit: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Return bounded metadata for one path on an enrolled executable device.
    PathInfo {
        #[arg(value_name = "DEVICE_OR_PATH", num_args = 1..=2)]
        operands: Vec<String>,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List paths matching a bounded relative glob under one allowed root.
    Glob {
        #[arg(value_name = "DEVICE_ROOT_PATTERN", num_args = 2..=3)]
        operands: Vec<String>,
        #[arg(long, default_value_t = 0)]
        cursor: u64,
        #[arg(long, default_value_t = 128)]
        limit: u32,
        #[arg(long, default_value_t = 32_768)]
        max_bytes: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Search bounded UTF-8 lines under one allowed root using a linear-time regex.
    Grep {
        #[arg(value_name = "DEVICE_ROOT_REGEX", num_args = 2..=3)]
        operands: Vec<String>,
        #[arg(long, value_name = "GLOB")]
        include: Option<String>,
        #[arg(long, default_value_t = 0)]
        cursor: u64,
        #[arg(long, default_value_t = 128)]
        limit: u32,
        #[arg(long, default_value_t = 32_768)]
        max_bytes: u32,
        #[arg(long, default_value_t = 8_388_608)]
        max_scan_bytes: u64,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Create or replace one bounded UTF-8 file with an explicit write precondition.
    Write {
        #[arg(value_name = "DEVICE_OR_PATH", num_args = 1..=2)]
        operands: Vec<String>,
        #[arg(long, value_name = "TEXT")]
        contents: String,
        #[arg(long)]
        create_only: bool,
        #[arg(long, value_name = "SHA256")]
        expected_sha256: Option<String>,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Replace one uniquely occurring text fragment under an expected file SHA-256.
    Edit {
        #[arg(value_name = "DEVICE_OR_PATH", num_args = 1..=2)]
        operands: Vec<String>,
        #[arg(long, value_name = "SHA256")]
        expected_sha256: String,
        #[arg(long, value_name = "TEXT")]
        old: String,
        #[arg(long, value_name = "TEXT")]
        new: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Manage one bounded live-session Shell task through the Controller.
    Shell {
        #[command(subcommand)]
        command: ShellCommand,
    },
    /// Manage Controller-owned local TCP forwards to an enrolled Target.
    Forward {
        #[command(subcommand)]
        command: ForwardCommand,
    },
    /// Manage Controller-owned loopback proxy adapters.
    Proxy {
        #[command(subcommand)]
        command: ProxyCommand,
    },
    /// Manage resumable single-file transfers owned by the Controller.
    File {
        #[command(subcommand)]
        command: FileCommand,
    },
    /// Serve Clew's agent tools over Model Context Protocol.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Rename an enrolled device in the Controller catalog.
    Rename {
        device_id: DeviceId,
        display_name: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Revoke a device and disconnect its current session.
    Revoke {
        device_id: DeviceId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show recent bounded Controller activity.
    Activity {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Clear local Controller activity history.
    ActivityClear {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Export an encrypted Controller identity/state backup. Passphrase comes from an environment variable.
    BackupExport {
        output: PathBuf,
        #[arg(long, default_value = "CLEW_BACKUP_PASSPHRASE")]
        passphrase_env: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Restore an encrypted Controller backup into an empty, stopped Controller state.
    BackupRestore {
        input: PathBuf,
        #[arg(long, default_value = "CLEW_BACKUP_PASSPHRASE")]
        passphrase_env: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show whether a restored Controller is still paused for Recovery Review.
    RecoveryStatus {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Confirm Recovery Review and allow restored DeviceKeys to reconnect.
    RecoveryConfirm {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Ask the running Controller to exit cleanly.
    Shutdown {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Query the running Controller through the Local API.
    Status {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List devices known to the running Controller.
    Devices {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show the current or most recent Controller session generation/path for one stable DeviceId.
    SessionPathInfo {
        #[arg(value_name = "DEVICE_ID")]
        device_id: DeviceId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Serve MCP over stdin/stdout. The Controller remains the only state owner.
    Stdio {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Serve MCP Streamable HTTP on a loopback-only listener.
    Http {
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:4877")]
        listen: SocketAddr,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ForwardCommand {
    /// Add a persistent Controller-owned loopback TCP listener. Optional device uses shared selector semantics.
    Add {
        #[arg(value_name = "DEVICE")]
        device: Option<String>,
        #[arg(long, value_name = "HOST:PORT")]
        dest: String,
        #[arg(long, default_value_t = 0)]
        listen_port: u16,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List Controller-owned local TCP forwards.
    List {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Remove one Controller-owned local TCP forward.
    Remove {
        forward_id: ForwardId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}
#[derive(Debug, Subcommand)]
enum ProxyCommand {
    /// Manage a SOCKS5 v5/no-auth/TCP-CONNECT loopback proxy.
    Socks5 {
        #[command(subcommand)]
        command: Socks5Command,
    },
    /// Manage a loopback HTTP CONNECT tunnel proxy.
    HttpConnect {
        #[command(subcommand)]
        command: HttpConnectCommand,
    },
}

#[derive(Debug, Subcommand)]
enum Socks5Command {
    /// Add a persistent Controller-owned SOCKS5 loopback listener.
    Add {
        #[arg(value_name = "DEVICE")]
        device: Option<String>,
        #[arg(long, default_value_t = 0)]
        listen_port: u16,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List Controller-owned SOCKS5 listeners.
    List {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Remove one Controller-owned SOCKS5 listener.
    Remove {
        proxy_id: ProxyId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}
#[derive(Debug, Subcommand)]
enum HttpConnectCommand {
    /// Add a persistent Controller-owned HTTP CONNECT loopback listener.
    Add {
        #[arg(value_name = "DEVICE")]
        device: Option<String>,
        #[arg(long, default_value_t = 0)]
        listen_port: u16,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List Controller-owned HTTP CONNECT listeners.
    List {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Remove one Controller-owned HTTP CONNECT listener.
    Remove {
        proxy_id: ProxyId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FileConflictArg {
    Fail,
    Replace,
    Rename,
}

impl From<FileConflictArg> for FileConflictPolicy {
    fn from(value: FileConflictArg) -> Self {
        match value {
            FileConflictArg::Fail => Self::FailIfExists,
            FileConflictArg::Replace => Self::ReplaceExisting,
            FileConflictArg::Rename => Self::RenameIfExists,
        }
    }
}

#[derive(Debug, Subcommand)]
enum FileCommand {
    /// Start one Controller-to-device upload. Use --directory for a bounded serial directory Put.
    Put {
        #[arg(value_name = "DEVICE")]
        device: Option<String>,
        #[arg(long, value_name = "LOCAL_PATH")]
        source: PathBuf,
        #[arg(long, value_name = "DEVICE_PATH")]
        dest: String,
        #[arg(long, default_value_t = 32_768)]
        chunk_size: u32,
        /// Treat SOURCE/DEST as a directory tree. Directory Put currently supports fail-if-exists only.
        #[arg(long)]
        directory: bool,
        #[arg(long, value_enum, default_value = "fail")]
        conflict: FileConflictArg,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Start one device-to-Controller single-file download. The optional device uses shared selector semantics.
    Get {
        #[arg(value_name = "DEVICE")]
        device: Option<String>,
        #[arg(long, value_name = "DEVICE_PATH")]
        source: String,
        #[arg(long, value_name = "LOCAL_FILE")]
        dest: PathBuf,
        #[arg(long, default_value_t = 32_768)]
        chunk_size: u32,
        #[arg(long, value_enum, default_value = "fail")]
        conflict: FileConflictArg,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show Controller-owned progress for one transfer. Use --directory for a directory Put.
    Status {
        transfer_id: TransferId,
        #[arg(long)]
        directory: bool,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Request bounded cancellation for one transfer. Use --directory for a directory Put.
    Cancel {
        transfer_id: TransferId,
        #[arg(long)]
        directory: bool,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ShellCommand {
    /// Start one Shell task. The optional first operand is a shared device selector.
    Start {
        #[arg(value_name = "DEVICE_OR_COMMAND", num_args = 1..=2)]
        operands: Vec<String>,
        #[arg(long, value_name = "DIR")]
        cwd: String,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long, default_value_t = 300_000)]
        timeout_ms: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show current state for a Controller-projected live Shell task.
    Status {
        task_id: TaskId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Fetch bounded stdout/stderr chunks using absolute byte cursors.
    Attach {
        task_id: TaskId,
        #[arg(long, default_value_t = 0)]
        stdout_offset: u64,
        #[arg(long, default_value_t = 0)]
        stderr_offset: u64,
        #[arg(long, default_value_t = 12_288)]
        max_bytes_per_stream: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Request cancellation of a live Shell task.
    Cancel {
        task_id: TaskId,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum OutfitCommand {
    /// List built-in and custom Outfit profiles.
    List {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Create a custom Outfit from a built-in preset.
    New {
        display_name: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value = "clew-original")]
        preset: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Clone any existing Outfit into an editable custom profile.
    Clone {
        source_id: String,
        display_name: String,
        #[arg(long)]
        id: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Show one complete Outfit profile as JSON.
    Show {
        outfit_id: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Update one supported field and create a new Outfit revision when it changes.
    Set {
        outfit_id: String,
        field: String,
        value: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// List imported content-addressed Outfit assets.
    Assets {
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Import one bounded PNG or SVG asset into the Controller-owned asset store.
    ImportAsset {
        path: PathBuf,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Bind one imported asset to app-icon, tray-icon, logo, or key-visual.
    SetAsset {
        outfit_id: String,
        slot: String,
        asset_id: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
    /// Set the Outfit selected by default for future invitation workflows.
    SetDefault {
        outfit_id: String,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("clew: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Host {
            site,
            state_dir,
            foreground,
            connector_only,
        } => {
            let launch_mode = if connector_only {
                HostLaunchMode::ConnectorOnly
            } else {
                HostLaunchMode::Default
            };
            run_host(site, state_dir, foreground, launch_mode).await?
        }
        Command::Controller { state_dir } => {
            let config = controller_config(state_dir)?;
            match start_controller(config).await? {
                ControllerStart::Primary(runtime) => {
                    let status = runtime.status().clone();
                    println!(
                        "Clew controller ready (pid {}, instance {}).",
                        status.pid, status.instance_id
                    );
                    runtime
                        .serve_until(async {
                            let _ = tokio::signal::ctrl_c().await;
                        })
                        .await?;
                }
                ControllerStart::Existing(status) => {
                    println!(
                        "Clew controller already running (pid {}, instance {}).",
                        status.pid, status.instance_id
                    );
                }
            }
        }
        Command::Gui { state_dir } => {
            #[cfg(any(windows, target_os = "macos"))]
            {
                let config = controller_config(state_dir)?;
                gui::run(config).await?;
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            {
                let _ = state_dir;
                return Err(
                    "Controller GUI is available on Windows and macOS; use `clew controller` on Linux"
                        .into(),
                );
            }
        }
        Command::Mint(MintArgs {
            site_name,
            outfit,
            roots,
            output,
            max_claims,
            valid_hours,
            deployment_hours,
            max_result_bytes,
            read_timeout_ms,
            allow_write,
            allow_shell,
            allow_tcp_egress,
            state_dir,
        }) => {
            let valid_for_ms = valid_hours
                .checked_mul(60 * 60 * 1_000)
                .ok_or("invite validity is too large")?;
            let deployment_window_ms = deployment_hours
                .checked_mul(60 * 60 * 1_000)
                .ok_or("deployment window is too large")?;
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let result = client
                .invite_issue(InviteIssueRequest {
                    site_name,
                    outfit_id: outfit,
                    roots: roots
                        .into_iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                    max_claims,
                    valid_for_ms,
                    deployment_window_ms,
                    max_result_bytes,
                    read_timeout_ms,
                    allow_write,
                    allow_shell,
                    allow_tcp_egress,
                })
                .await?;
            invite_io::write_invitation(&client, &result.site_file, &output).await?;
            println!("{}", output.display());
        }
        Command::Outfit { command } => run_outfit_command(command).await?,
        Command::InviteClose {
            invite_id,
            state_dir,
        } => {
            let config = controller_config(state_dir)?;
            LocalApiClient::new(config).invite_close(invite_id).await?;
        }
        Command::Read {
            operands,
            offset,
            limit,
            state_dir,
        } => {
            let (selector, path) = match operands.as_slice() {
                [path] => (None, path.clone()),
                [selector, path] => (Some(selector.as_str()), path.clone()),
                _ => unreachable!("clap enforces one or two read operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let result = client
                .read(RemoteReadRequest {
                    device_id,
                    path,
                    offset,
                    limit,
                })
                .await?;
            std::io::stdout().write_all(&result.data)?;
        }
        Command::PathInfo {
            operands,
            state_dir,
        } => {
            let (selector, path) = match operands.as_slice() {
                [path] => (None, path.clone()),
                [selector, path] => (Some(selector.as_str()), path.clone()),
                _ => unreachable!("clap enforces one or two path-info operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let info = client
                .path_info(RemotePathInfoRequest { device_id, path })
                .await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Glob {
            operands,
            cursor,
            limit,
            max_bytes,
            state_dir,
        } => {
            let (selector, root, pattern) = match operands.as_slice() {
                [root, pattern] => (None, root.clone(), pattern.clone()),
                [selector, root, pattern] => {
                    (Some(selector.as_str()), root.clone(), pattern.clone())
                }
                _ => unreachable!("clap enforces two or three glob operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let page = client
                .glob(RemoteGlobRequest {
                    device_id,
                    root,
                    pattern,
                    cursor,
                    limit,
                    max_bytes,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Command::Grep {
            operands,
            include,
            cursor,
            limit,
            max_bytes,
            max_scan_bytes,
            state_dir,
        } => {
            let (selector, root, pattern) = match operands.as_slice() {
                [root, pattern] => (None, root.clone(), pattern.clone()),
                [selector, root, pattern] => {
                    (Some(selector.as_str()), root.clone(), pattern.clone())
                }
                _ => unreachable!("clap enforces two or three grep operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let page = client
                .grep(RemoteGrepRequest {
                    device_id,
                    root,
                    pattern,
                    include,
                    cursor,
                    limit,
                    max_bytes,
                    max_scan_bytes,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&page)?);
        }
        Command::Write {
            operands,
            contents,
            create_only,
            expected_sha256,
            state_dir,
        } => {
            let (selector, path) = match operands.as_slice() {
                [path] => (None, path.clone()),
                [selector, path] => (Some(selector.as_str()), path.clone()),
                _ => unreachable!("clap enforces one or two write operands"),
            };
            let precondition = match (create_only, expected_sha256) {
                (true, None) => FsWritePrecondition::CreateOnly,
                (false, Some(hash)) => FsWritePrecondition::MatchSha256(hash),
                _ => {
                    return Err(
                        "write requires exactly one of --create-only or --expected-sha256".into(),
                    );
                }
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let result = client
                .write(RemoteWriteRequest {
                    device_id,
                    path,
                    contents,
                    precondition,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Edit {
            operands,
            expected_sha256,
            old,
            new,
            state_dir,
        } => {
            let (selector, path) = match operands.as_slice() {
                [path] => (None, path.clone()),
                [selector, path] => (Some(selector.as_str()), path.clone()),
                _ => unreachable!("clap enforces one or two edit operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let result = client
                .edit(RemoteEditRequest {
                    device_id,
                    path,
                    expected_sha256,
                    old,
                    new,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Shell { command } => run_shell_command(command).await?,
        Command::Forward { command } => match command {
            ForwardCommand::Add {
                device,
                dest,
                listen_port,
                state_dir,
            } => {
                let (dest_host, dest_port) = parse_host_port(&dest)?;
                let client = LocalApiClient::new(controller_config(state_dir)?);
                let devices = client.device_list().await?;
                let device_id = select_executable_device(&devices.devices, device.as_deref())?;
                let info = client
                    .forward_add(ForwardAddRequest {
                        device_id,
                        listen_port,
                        dest_host,
                        dest_port,
                    })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            ForwardCommand::List { state_dir } => {
                let result = LocalApiClient::new(controller_config(state_dir)?)
                    .forward_list()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ForwardCommand::Remove {
                forward_id,
                state_dir,
            } => {
                let info = LocalApiClient::new(controller_config(state_dir)?)
                    .forward_remove(forward_id)
                    .await?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        },
        Command::Proxy { command } => match command {
            ProxyCommand::Socks5 { command } => match command {
                Socks5Command::Add {
                    device,
                    listen_port,
                    state_dir,
                } => {
                    let client = LocalApiClient::new(controller_config(state_dir)?);
                    let devices = client.device_list().await?;
                    let device_id = select_executable_device(&devices.devices, device.as_deref())?;
                    let info = client
                        .socks5_add(Socks5AddRequest {
                            device_id,
                            listen_port,
                        })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                Socks5Command::List { state_dir } => {
                    let result = LocalApiClient::new(controller_config(state_dir)?)
                        .socks5_list()
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                Socks5Command::Remove {
                    proxy_id,
                    state_dir,
                } => {
                    let info = LocalApiClient::new(controller_config(state_dir)?)
                        .socks5_remove(proxy_id)
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            },
            ProxyCommand::HttpConnect { command } => match command {
                HttpConnectCommand::Add {
                    device,
                    listen_port,
                    state_dir,
                } => {
                    let client = LocalApiClient::new(controller_config(state_dir)?);
                    let devices = client.device_list().await?;
                    let device_id = select_executable_device(&devices.devices, device.as_deref())?;
                    let info = client
                        .http_connect_add(HttpConnectAddRequest {
                            device_id,
                            listen_port,
                        })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                HttpConnectCommand::List { state_dir } => {
                    let result = LocalApiClient::new(controller_config(state_dir)?)
                        .http_connect_list()
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                HttpConnectCommand::Remove {
                    proxy_id,
                    state_dir,
                } => {
                    let info = LocalApiClient::new(controller_config(state_dir)?)
                        .http_connect_remove(proxy_id)
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            },
        },
        Command::File { command } => match command {
            FileCommand::Put {
                device,
                source,
                dest,
                chunk_size,
                directory,
                conflict,
                state_dir,
            } => {
                let source = std::fs::canonicalize(&source)?;
                let source_path = source
                    .to_str()
                    .ok_or("Controller source path must be valid UTF-8")?
                    .to_owned();
                let client = LocalApiClient::new(controller_config(state_dir)?);
                let devices = client.device_list().await?;
                let device_id = select_executable_device(&devices.devices, device.as_deref())?;
                if directory {
                    if !source.is_dir() {
                        return Err("Controller directory source must be a directory".into());
                    }
                    if !matches!(conflict, FileConflictArg::Fail) {
                        return Err("directory Put currently supports --conflict fail only".into());
                    }
                    let info = client
                        .directory_put(RemoteDirectoryPutRequest {
                            device_id,
                            source_path,
                            device_root: dest,
                            chunk_size,
                        })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    let info = client
                        .file_put(RemoteFilePutRequest {
                            device_id,
                            source_path,
                            device_path: dest,
                            chunk_size,
                            conflict_policy: conflict.into(),
                        })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            }
            FileCommand::Get {
                device,
                source,
                dest,
                chunk_size,
                conflict,
                state_dir,
            } => {
                let destination = if dest.is_absolute() {
                    dest
                } else {
                    std::env::current_dir()?.join(dest)
                };
                let destination_path = destination
                    .to_str()
                    .ok_or("Controller destination path must be valid UTF-8")?
                    .to_owned();
                let client = LocalApiClient::new(controller_config(state_dir)?);
                let devices = client.device_list().await?;
                let device_id = select_executable_device(&devices.devices, device.as_deref())?;
                let info = client
                    .file_get(RemoteFileGetRequest {
                        device_id,
                        device_path: source,
                        destination_path,
                        chunk_size,
                        conflict_policy: conflict.into(),
                    })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            FileCommand::Status {
                transfer_id,
                directory,
                state_dir,
            } => {
                let client = LocalApiClient::new(controller_config(state_dir)?);
                if directory {
                    let info = client.directory_status(transfer_id).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    let info = client.file_status(transfer_id).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            }
            FileCommand::Cancel {
                transfer_id,
                directory,
                state_dir,
            } => {
                let client = LocalApiClient::new(controller_config(state_dir)?);
                if directory {
                    let info = client.directory_cancel(transfer_id).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                } else {
                    let info = client.file_cancel(transfer_id).await?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
            }
        },
        Command::Mcp { command } => match command {
            McpCommand::Stdio { state_dir } => {
                mcp::serve_stdio(controller_config(state_dir)?).await?;
            }
            McpCommand::Http { listen, state_dir } => {
                mcp::serve_http(controller_config(state_dir)?, listen).await?;
            }
        },
        Command::Rename {
            device_id,
            display_name,
            state_dir,
        } => {
            let config = controller_config(state_dir)?;
            let device = LocalApiClient::new(config)
                .device_rename(device_id, display_name)
                .await?;
            println!("{}", serde_json::to_string_pretty(&device)?);
        }
        Command::Revoke {
            device_id,
            state_dir,
        } => {
            let config = controller_config(state_dir)?;
            LocalApiClient::new(config).device_revoke(device_id).await?;
        }
        Command::Activity { limit, state_dir } => {
            let config = controller_config(state_dir)?;
            let activity = LocalApiClient::new(config).activity_list(limit).await?;
            println!("{}", serde_json::to_string_pretty(&activity)?);
        }
        Command::ActivityClear { state_dir } => {
            let config = controller_config(state_dir)?;
            LocalApiClient::new(config).activity_clear().await?;
        }
        Command::BackupExport {
            output,
            passphrase_env,
            state_dir,
        } => {
            let passphrase = backup_passphrase(&passphrase_env)?;
            let path = output
                .to_str()
                .ok_or("backup output path must be valid UTF-8")?
                .to_owned();
            let config = controller_config(state_dir)?;
            LocalApiClient::new(config)
                .backup_export(BackupExportRequest { path, passphrase })
                .await?;
            println!("{}", output.display());
        }
        Command::BackupRestore {
            input,
            passphrase_env,
            state_dir,
        } => {
            let passphrase = backup_passphrase(&passphrase_env)?;
            let config = controller_config(state_dir)?;
            let review = restore_controller_backup(&config, &input, &passphrase)?;
            println!("{}", serde_json::to_string_pretty(&review)?);
        }
        Command::RecoveryStatus { state_dir } => {
            let config = controller_config(state_dir)?;
            let status = LocalApiClient::new(config).recovery_status().await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::RecoveryConfirm { state_dir } => {
            let config = controller_config(state_dir)?;
            let status = LocalApiClient::new(config).recovery_confirm().await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Shutdown { state_dir } => {
            let config = controller_config(state_dir)?;
            LocalApiClient::new(config).controller_shutdown().await?;
        }
        Command::Status { state_dir } => {
            let config = controller_config(state_dir)?;
            let status = LocalApiClient::new(config).controller_status().await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Devices { state_dir } => {
            let config = controller_config(state_dir)?;
            let devices = LocalApiClient::new(config).device_list().await?;
            println!("{}", serde_json::to_string_pretty(&devices)?);
        }
        Command::SessionPathInfo {
            device_id,
            state_dir,
        } => {
            let config = controller_config(state_dir)?;
            let info = LocalApiClient::new(config)
                .session_path_info(device_id)
                .await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
    }
    Ok(())
}

async fn run_shell_command(command: ShellCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ShellCommand::Start {
            operands,
            cwd,
            env,
            timeout_ms,
            state_dir,
        } => {
            let (selector, command) = match operands.as_slice() {
                [command] => (None, command.clone()),
                [selector, command] => (Some(selector.as_str()), command.clone()),
                _ => unreachable!("clap enforces one or two shell start operands"),
            };
            let config = controller_config(state_dir)?;
            let client = LocalApiClient::new(config);
            let devices = client.device_list().await?;
            let device_id = select_executable_device(&devices.devices, selector)?;
            let task_id = client
                .shell_start(RemoteShellStartRequest {
                    device_id,
                    command,
                    cwd,
                    env: parse_shell_env(env)?,
                    timeout_ms,
                })
                .await?;
            println!("{task_id}");
        }
        ShellCommand::Status { task_id, state_dir } => {
            let status = LocalApiClient::new(controller_config(state_dir)?)
                .shell_status(task_id)
                .await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        ShellCommand::Attach {
            task_id,
            stdout_offset,
            stderr_offset,
            max_bytes_per_stream,
            state_dir,
        } => {
            let output = LocalApiClient::new(controller_config(state_dir)?)
                .shell_attach(RemoteShellAttachRequest {
                    task_id,
                    stdout_offset,
                    stderr_offset,
                    max_bytes_per_stream,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        ShellCommand::Cancel { task_id, state_dir } => {
            LocalApiClient::new(controller_config(state_dir)?)
                .shell_cancel(task_id)
                .await?;
        }
    }
    Ok(())
}

fn parse_shell_env(entries: Vec<String>) -> Result<BTreeMap<String, String>, std::io::Error> {
    let mut env = BTreeMap::new();
    for entry in entries {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--env must use KEY=VALUE",
            ));
        };
        if env.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("duplicate --env key: {key}"),
            ));
        }
    }
    Ok(env)
}

async fn run_outfit_command(command: OutfitCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        OutfitCommand::List { state_dir } => {
            let result = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_list()
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutfitCommand::New {
            display_name,
            id,
            preset,
            state_dir,
        } => {
            let profile = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_create(OutfitCreateRequest {
                    outfit_id: id,
                    display_name,
                    preset: parse_outfit_preset(&preset)?,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        OutfitCommand::Clone {
            source_id,
            display_name,
            id,
            state_dir,
        } => {
            let profile = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_clone(OutfitCloneRequest {
                    source_id,
                    outfit_id: id,
                    display_name,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        OutfitCommand::Show {
            outfit_id,
            state_dir,
        } => {
            let profile = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_show(outfit_id)
                .await?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        OutfitCommand::Set {
            outfit_id,
            field,
            value,
            state_dir,
        } => {
            let profile = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_set_field(OutfitSetFieldRequest {
                    outfit_id,
                    field,
                    value,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        OutfitCommand::Assets { state_dir } => {
            let assets = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_asset_list()
                .await?;
            println!("{}", serde_json::to_string_pretty(&assets)?);
        }
        OutfitCommand::ImportAsset { path, state_dir } => {
            let path = path
                .to_str()
                .ok_or("asset path must be valid UTF-8")?
                .to_owned();
            let asset = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_asset_import(OutfitAssetImportRequest { path })
                .await?;
            println!("{}", serde_json::to_string_pretty(&asset)?);
        }
        OutfitCommand::SetAsset {
            outfit_id,
            slot,
            asset_id,
            state_dir,
        } => {
            let profile = LocalApiClient::new(controller_config(state_dir)?)
                .outfit_set_asset(OutfitSetAssetRequest {
                    outfit_id,
                    slot,
                    asset_id,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        OutfitCommand::SetDefault {
            outfit_id,
            state_dir,
        } => {
            LocalApiClient::new(controller_config(state_dir)?)
                .outfit_set_default(outfit_id)
                .await?;
        }
    }
    Ok(())
}

fn parse_outfit_preset(value: &str) -> Result<OutfitPreset, Box<dyn std::error::Error>> {
    match value {
        "clew-original" => Ok(OutfitPreset::ClewOriginal),
        "research-lab" => Ok(OutfitPreset::ResearchLab),
        "friendly-minimal" => Ok(OutfitPreset::FriendlyMinimal),
        "institution-clean" => Ok(OutfitPreset::InstitutionClean),
        _ => Err(format!(
            "unknown Outfit preset {value:?}; expected clew-original, research-lab, friendly-minimal, or institution-clean"
        )
        .into()),
    }
}

async fn run_host(
    site: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    foreground: bool,
    launch_mode: HostLaunchMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let layout = controller_config(state_dir)?.state_layout();
    let context = HostLaunchContext::current(site, layout.clone())?;
    let state = resolve_host_launch_with_mode(context.clone(), launch_mode)?;

    if foreground {
        return run_host_foreground(&layout, state).await;
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    return run_host_foreground(&layout, state).await;

    #[cfg(any(windows, target_os = "macos"))]
    return run_host_desktop(layout, context, state, launch_mode).await;
}

async fn wait_for_host_shutdown(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
async fn run_host_network_lifecycle(
    layout: clew_core::StateLayout,
    state: HostLaunchState,
    shutdown: tokio::sync::watch::Receiver<bool>,
    state_tx: Option<tokio::sync::mpsc::UnboundedSender<HostLaunchState>>,
) -> Result<(), clew_host::HostRemoteError> {
    let Some(state) = wait_for_networked_activation_until(&layout, state, shutdown.clone()).await?
    else {
        return Ok(());
    };
    if let Some(state_tx) = &state_tx {
        let _ = state_tx.send(state.clone());
    }
    if let HostLaunchState::Active { membership, .. } = state
        && membership.marker.controller_endpoint.is_some()
    {
        return serve_networked_membership_until_with_layout(
            &layout,
            &membership,
            wait_for_host_shutdown(shutdown),
        )
        .await;
    }
    wait_for_host_shutdown(shutdown).await;
    Ok(())
}

async fn run_host_foreground(
    layout: &clew_core::StateLayout,
    state: HostLaunchState,
) -> Result<(), Box<dyn std::error::Error>> {
    let key = state.instance_key()?;
    let instance = match acquire_host_instance(layout, key).await? {
        HostInstanceStart::ExistingWoken => {
            println!("Clew host already running; requested the existing window to show.");
            return Ok(());
        }
        HostInstanceStart::Primary(instance) => instance,
    };
    print_host_state(&state);
    if matches!(
        state,
        HostLaunchState::MissingInvite { .. } | HostLaunchState::AmbiguousMembership { .. }
    ) {
        return Ok(());
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server =
        tokio::spawn(instance.serve_until(wait_for_host_shutdown(shutdown_rx.clone()), None));
    let ctrl_shutdown = shutdown_tx.clone();
    let ctrl_c = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = ctrl_shutdown.send(true);
    });

    let result = async {
        let Some(state) =
            wait_for_networked_activation_until(layout, state, shutdown_rx.clone()).await?
        else {
            return Ok::<(), clew_host::HostRemoteError>(());
        };
        print_host_state(&state);
        if let HostLaunchState::Active { membership, .. } = state
            && membership.marker.controller_endpoint.is_some()
        {
            serve_networked_membership_until_with_layout(
                layout,
                &membership,
                wait_for_host_shutdown(shutdown_rx.clone()),
            )
            .await?;
        } else {
            wait_for_host_shutdown(shutdown_rx.clone()).await;
        }
        Ok(())
    }
    .await;

    let _ = shutdown_tx.send(true);
    ctrl_c.abort();
    let _ = ctrl_c.await;
    server.await??;
    result?;
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
async fn run_host_desktop(
    layout: clew_core::StateLayout,
    mut context: HostLaunchContext,
    mut state: HostLaunchState,
    launch_mode: HostLaunchMode,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let key = state.instance_key()?;
        let instance = match acquire_host_instance(&layout, key).await? {
            HostInstanceStart::ExistingWoken => {
                println!("Clew host already running; requested the existing window to show.");
                return Ok(());
            }
            HostInstanceStart::Primary(instance) => instance,
        };
        let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (instance_shutdown_tx, instance_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(instance.serve_until(
            async move {
                let _ = instance_shutdown_rx.await;
            },
            Some(wake_tx),
        ));

        let (network_shutdown_tx, network_shutdown_rx) = tokio::sync::watch::channel(false);
        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let network = if matches!(
            state,
            HostLaunchState::Active { .. } | HostLaunchState::AwaitingEnrollment { .. }
        ) {
            let layout = layout.clone();
            let network_state = state.clone();
            Some(tokio::spawn(async move {
                run_host_network_lifecycle(
                    layout,
                    network_state,
                    network_shutdown_rx,
                    Some(state_tx),
                )
                .await
            }))
        } else {
            drop(state_tx);
            None
        };

        let action = host_gui::run(&layout, state, wake_rx, state_rx)?;
        let _ = network_shutdown_tx.send(true);
        let _ = instance_shutdown_tx.send(());
        server.await??;
        if let Some(network) = network {
            network.await??;
        }
        match action {
            host_gui::HostGuiAction::Exit => return Ok(()),
            host_gui::HostGuiAction::OpenSite(path) => {
                context.explicit_site = Some(path);
                state = resolve_host_launch_with_mode(context.clone(), launch_mode)?;
            }
            host_gui::HostGuiAction::SelectMembership {
                controller_id,
                site_id,
            } => {
                let membership = HostMembershipStore::new(layout.clone())
                    .load(controller_id, site_id)?
                    .ok_or("selected Clew membership no longer exists")?;
                context.explicit_site = None;
                state = HostLaunchState::Active {
                    membership,
                    source: HostSiteSource::LocalMembership,
                };
            }
        }
    }
}

fn print_host_state(state: &HostLaunchState) {
    match state {
        HostLaunchState::Active { membership, .. } if state.is_connector_only() => {
            println!(
                "Clew connection helper ready: {} / {}.",
                membership.marker.site_name, membership.device.display_name
            );
            println!(
                "This computer only helps nearby computers connect; its files and commands are not exposed."
            );
        }
        HostLaunchState::Active { membership, .. } => println!(
            "Clew host ready: {} / {} (DeviceId {}).",
            membership.marker.site_name,
            membership.device.display_name,
            membership.marker.device_id
        ),
        HostLaunchState::AwaitingEnrollment {
            site_file,
            hostname,
            ..
        } if state.is_connector_only() => {
            println!(
                "Clew invitation verified: {} / {}; connecting only as a nearby connection helper.",
                site_file.payload.bootstrap.payload.site_name, hostname
            );
            println!("This computer will not expose its files or commands.");
        }
        HostLaunchState::AwaitingEnrollment {
            site_file,
            hostname,
            ..
        } => println!(
            "Clew invitation verified: {} / {}; waiting for Controller enrollment.",
            site_file.payload.bootstrap.payload.site_name, hostname
        ),
        HostLaunchState::MissingInvite { view, .. } => {
            println!("{}", view.title);
            println!("{}", view.body);
            if let Some(extract) = &view.extract_first {
                println!("{extract}");
            }
        }
        HostLaunchState::AmbiguousMembership { candidates, .. } => {
            println!("Found multiple Clew memberships; choose one in the desktop Host UI:");
            for candidate in candidates {
                println!("- {} / {}", candidate.site_name, candidate.device_id);
            }
        }
    }
}

fn parse_host_port(value: &str) -> Result<(String, u16), Box<dyn std::error::Error>> {
    let value = value.trim();
    if value.is_empty() {
        return Err("TCP destination cannot be empty".into());
    }
    if let Ok(socket) = value.parse::<SocketAddr>() {
        return Ok((socket.ip().to_string(), socket.port()));
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err("TCP destination must be HOST:PORT (IPv6 literals require [addr]:port)".into());
    };
    if host.is_empty() || host.contains(':') {
        return Err("TCP destination must be HOST:PORT (IPv6 literals require [addr]:port)".into());
    }
    let port: u16 = port.parse()?;
    if port == 0 {
        return Err("TCP destination port must be nonzero".into());
    }
    Ok((host.to_owned(), port))
}
fn backup_passphrase(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    if name.is_empty() {
        return Err("backup passphrase environment variable name cannot be empty".into());
    }
    std::env::var(name)
        .map_err(|_| format!("backup passphrase environment variable {name} is not set").into())
}

fn controller_config(
    state_dir: Option<PathBuf>,
) -> Result<ControllerConfig, Box<dyn std::error::Error>> {
    match state_dir {
        Some(path) => Ok(ControllerConfig::new(path)),
        None => Ok(ControllerConfig::for_current_user()?),
    }
}
