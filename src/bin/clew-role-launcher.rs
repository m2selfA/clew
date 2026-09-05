#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const ROLE_FILE: &str = "role-hint.clew";
const USE_THIS_MACHINE: &[u8] = b"use-this-machine\n";
const CONNECTOR_ONLY: &[u8] = b"connector-only\n";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    UseThisMachine,
    ConnectorOnly,
}

fn main() -> ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "Clew Site Kit launcher\n\nThis signed launcher is intended to be opened from a generated Clew Site Kit."
        );
        return ExitCode::SUCCESS;
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("clew-role-launcher {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        eprintln!("Clew Site Kit launcher does not accept command-line arguments");
        return ExitCode::from(64);
    }
    match launch() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            report_error(&format!("Clew Site Kit could not start: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn report_error(message: &str) {
    eprintln!("{message}");
    #[cfg(any(windows, target_os = "macos"))]
    {
        let _ = rfd::MessageDialog::new()
            .set_title("Clew Site Kit")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

fn launch() -> Result<u8, Box<dyn std::error::Error>> {
    let role_dir = role_directory()?;
    let root = role_dir
        .parent()
        .ok_or("Site Kit role launcher has no package root")?;
    let role = read_role(&role_dir.join(ROLE_FILE))?;
    let site = root.join("site.clew");
    require_regular_file(&site, "site.clew")?;
    let runtime = runtime_path(root);
    require_regular_file(&runtime, "Clew runtime")?;

    let mut command = Command::new(runtime);
    command.arg("host").arg("--site").arg(&site);
    if role == Role::ConnectorOnly {
        command.arg("--connector-only");
    }

    #[cfg(windows)]
    {
        command
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn()?;
        return Ok(0);
    }

    #[cfg(target_os = "macos")]
    {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn()?;
        return Ok(0);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = command.status()?;
        return Ok(status.code().unwrap_or(1).clamp(0, 255) as u8);
    }

    #[allow(unreachable_code)]
    Err("unsupported Site Kit launcher platform".into())
}

fn role_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    #[cfg(target_os = "macos")]
    {
        let app = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
            .ok_or("Site Kit macOS launcher is outside an app bundle")?;
        return app
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Site Kit macOS launcher app has no role directory".into());
    }
    #[cfg(not(target_os = "macos"))]
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Site Kit launcher has no role directory".into())
}

fn read_role(path: &Path) -> Result<Role, Box<dyn std::error::Error>> {
    require_regular_file(path, ROLE_FILE)?;
    let bytes = fs::read(path)?;
    match bytes.as_slice() {
        USE_THIS_MACHINE => Ok(Role::UseThisMachine),
        CONNECTOR_ONLY => Ok(Role::ConnectorOnly),
        _ => Err("Site Kit role marker is invalid".into()),
    }
}

fn require_regular_file(path: &Path, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} is not a regular file").into());
    }
    Ok(())
}

#[cfg(windows)]
fn runtime_path(root: &Path) -> PathBuf {
    root.join(".clew-runtime").join("clew.exe")
}

#[cfg(target_os = "macos")]
fn runtime_path(root: &Path) -> PathBuf {
    root.join(".clew-runtime")
        .join("Clew.app")
        .join("Contents")
        .join("Resources")
        .join("clew")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn runtime_path(root: &Path) -> PathBuf {
    root.join(".clew-runtime").join("clew")
}
