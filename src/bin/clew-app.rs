use std::process::ExitCode;

#[cfg(target_os = "macos")]
use std::{env, process::Command};

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    match run_controller_app() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("Clew could not start: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
fn run_controller_app() -> Result<u8, Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(|path| path.parent())
        .ok_or("Clew.app launcher is outside Contents/MacOS")?;
    let cli = contents.join("Resources").join("clew");
    let forwarded = env::args_os()
        .skip(1)
        .filter(|arg| !arg.to_string_lossy().starts_with("-psn_"));
    let status = Command::new(cli).arg("gui").args(forwarded).status()?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("clew-app is a macOS bundle launcher and is not supported on this platform");
    ExitCode::from(64)
}
