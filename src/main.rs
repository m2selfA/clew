use std::{path::PathBuf, process::ExitCode};

#[cfg(any(windows, target_os = "macos"))]
mod gui;
#[cfg(any(windows, target_os = "macos"))]
mod host_gui;

use clap::{Parser, Subcommand};
use clew_host::{
    HostInstanceStart, HostLaunchContext, HostLaunchState, acquire_host_instance,
    resolve_host_launch,
};
#[cfg(any(windows, target_os = "macos"))]
use clew_host::{HostMembershipStore, HostSiteSource};
use clew_runtime::{ControllerConfig, ControllerStart, LocalApiClient, start_controller};

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
    print_host_state(&state);
    if matches!(
        state,
        HostLaunchState::MissingInvite { .. } | HostLaunchState::AmbiguousMembership { .. }
    ) {
        return Ok(());
    }
    instance
        .serve_until(
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
            None,
        )
        .await?;
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
        let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(instance.serve_until(
            async move {
                let _ = shutdown_rx.await;
            },
            Some(wake_tx),
        ));
        let action = host_gui::run(state, wake_rx)?;
        let _ = shutdown_tx.send(());
        server.await??;
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

fn controller_config(
    state_dir: Option<PathBuf>,
) -> Result<ControllerConfig, Box<dyn std::error::Error>> {
    match state_dir {
        Some(path) => Ok(ControllerConfig::new(path)),
        None => Ok(ControllerConfig::for_current_user()?),
    }
}
