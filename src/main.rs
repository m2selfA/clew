use std::{path::PathBuf, process::ExitCode};

#[cfg(any(windows, target_os = "macos"))]
mod gui;

use clap::{Parser, Subcommand};
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

fn controller_config(
    state_dir: Option<PathBuf>,
) -> Result<ControllerConfig, Box<dyn std::error::Error>> {
    match state_dir {
        Some(path) => Ok(ControllerConfig::new(path)),
        None => Ok(ControllerConfig::for_current_user()?),
    }
}
