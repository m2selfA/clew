use std::{io::Write, path::PathBuf, process::ExitCode};

#[cfg(any(windows, target_os = "macos"))]
mod gui;
#[cfg(any(windows, target_os = "macos"))]
mod host_gui;

use clap::{Parser, Subcommand};
use clew_core::{DeviceId, InviteId};
use clew_host::{
    HostInstanceStart, HostLaunchContext, HostLaunchState, OutfitPreset, acquire_host_instance,
    complete_networked_activation, resolve_host_launch, serve_networked_membership_until,
};
#[cfg(any(windows, target_os = "macos"))]
use clew_host::{HostMembershipStore, HostSiteSource};
use clew_runtime::{
    BackupExportRequest, ControllerConfig, ControllerStart, InviteIssueRequest, LocalApiClient,
    OutfitCloneRequest, OutfitCreateRequest, OutfitSetFieldRequest, RemoteReadRequest,
    restore_controller_backup, start_controller,
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
    Mint {
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
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
    },
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
    Read {
        device_id: DeviceId,
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 16_384)]
        limit: u32,
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
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
        } => run_host(site, state_dir, foreground).await?,
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
        Command::Mint {
            site_name,
            outfit,
            roots,
            output,
            max_claims,
            valid_hours,
            deployment_hours,
            max_result_bytes,
            read_timeout_ms,
            state_dir,
        } => {
            let valid_for_ms = valid_hours
                .checked_mul(60 * 60 * 1_000)
                .ok_or("invite validity is too large")?;
            let deployment_window_ms = deployment_hours
                .checked_mul(60 * 60 * 1_000)
                .ok_or("deployment window is too large")?;
            let config = controller_config(state_dir)?;
            let result = LocalApiClient::new(config)
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
                })
                .await?;
            if let Some(parent) = output.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            result.site_file.write(&output)?;
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
            device_id,
            path,
            offset,
            limit,
            state_dir,
        } => {
            let config = controller_config(state_dir)?;
            let result = LocalApiClient::new(config)
                .read(RemoteReadRequest {
                    device_id,
                    path,
                    offset,
                    limit,
                })
                .await?;
            std::io::stdout().write_all(&result.data)?;
        }
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
    }
    Ok(())
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
) -> Result<(), Box<dyn std::error::Error>> {
    let layout = controller_config(state_dir)?.state_layout();
    let context = HostLaunchContext::current(site, layout.clone())?;
    let state = resolve_host_launch(context.clone())?;

    if foreground {
        return run_host_foreground(&layout, state).await;
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    return run_host_foreground(&layout, state).await;

    #[cfg(any(windows, target_os = "macos"))]
    return run_host_desktop(layout, context, state).await;
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
    let state = complete_networked_activation(layout, state).await?;
    print_host_state(&state);
    if matches!(
        state,
        HostLaunchState::MissingInvite { .. } | HostLaunchState::AmbiguousMembership { .. }
    ) {
        return Ok(());
    }

    let remote = if let HostLaunchState::Active { membership, .. } = &state
        && membership.marker.controller_endpoint.is_some()
        && membership.marker.read_policy.is_some()
    {
        let membership = membership.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            serve_networked_membership_until(&membership, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });
        Some((shutdown_tx, task))
    } else {
        None
    };

    let serve_result = instance
        .serve_until(
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
            None,
        )
        .await;
    if let Some((shutdown_tx, task)) = remote {
        let _ = shutdown_tx.send(());
        task.await??;
    }
    serve_result?;
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
async fn run_host_desktop(
    layout: clew_core::StateLayout,
    mut context: HostLaunchContext,
    mut state: HostLaunchState,
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
        state = complete_networked_activation(&layout, state).await?;
        let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(instance.serve_until(
            async move {
                let _ = shutdown_rx.await;
            },
            Some(wake_tx),
        ));
        let remote = if let HostLaunchState::Active { membership, .. } = &state
            && membership.marker.controller_endpoint.is_some()
            && membership.marker.read_policy.is_some()
        {
            let membership = membership.clone();
            let (remote_shutdown_tx, remote_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                serve_networked_membership_until(&membership, async move {
                    let _ = remote_shutdown_rx.await;
                })
                .await
            });
            Some((remote_shutdown_tx, task))
        } else {
            None
        };
        let action = host_gui::run(state, wake_rx)?;
        let _ = shutdown_tx.send(());
        server.await??;
        if let Some((remote_shutdown_tx, task)) = remote {
            let _ = remote_shutdown_tx.send(());
            task.await??;
        }
        match action {
            host_gui::HostGuiAction::Exit => return Ok(()),
            host_gui::HostGuiAction::OpenSite(path) => {
                context.explicit_site = Some(path);
                state = resolve_host_launch(context.clone())?;
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
