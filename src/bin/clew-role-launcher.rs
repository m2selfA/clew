#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(any(windows, target_os = "macos"))]
use std::process::Stdio;

#[cfg(not(windows))]
const ROLE_FILE: &str = "role-hint.clew";
#[cfg(not(windows))]
const USE_THIS_MACHINE: &[u8] = b"use-this-machine\n";
#[cfg(not(windows))]
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
            "Clew desktop launcher\n\nDouble-click this launcher. In a Site Kit it asks whether to use this computer or only help nearby computers connect; in a local release it opens the Controller GUI."
        );
        return ExitCode::SUCCESS;
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("clew-launcher {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        eprintln!("Clew desktop launcher does not accept command-line arguments");
        return ExitCode::from(64);
    }
    match launch() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            report_error(&format!("Clew could not start: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn report_error(message: &str) {
    eprintln!("{message}");
    #[cfg(any(windows, target_os = "macos"))]
    {
        let _ = rfd::MessageDialog::new()
            .set_title("Clew")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

#[cfg(windows)]
fn launch() -> Result<u8, Box<dyn std::error::Error>> {
    let root = env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or("Clew launcher has no package root")?;
    let site_path = root.join("site.clew");
    let site = site_path.exists().then_some(site_path);
    if let Some(site) = &site {
        require_regular_file(site, "site.clew")?;
    }
    let runtime = if site.is_some() {
        root.join(".clew-runtime").join("clew.exe")
    } else {
        root.join("clew.exe")
    };
    require_regular_file(&runtime, "Clew runtime")?;

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([520.0, 360.0])
            .with_min_inner_size([460.0, 320.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "Clew",
        options,
        Box::new(move |_cc| Ok(Box::new(WindowsLauncherApp::new(runtime, site)))),
    )?;
    Ok(0)
}

#[cfg(windows)]
struct WindowsLauncherApp {
    runtime: PathBuf,
    site: Option<PathBuf>,
    error: Option<String>,
}

#[cfg(windows)]
impl WindowsLauncherApp {
    fn new(runtime: PathBuf, site: Option<PathBuf>) -> Self {
        Self {
            runtime,
            site,
            error: None,
        }
    }

    fn start(&mut self, role: Option<Role>) -> Result<(), Box<dyn std::error::Error>> {
        let mut command = Command::new(&self.runtime);
        if let Some(site) = &self.site {
            command.arg("host").arg("--site").arg(site);
            if role == Some(Role::ConnectorOnly) {
                command.arg("--connector-only");
            }
        } else {
            command.arg("gui").env("CLEW_GUI_RUNTIME", "1");
        }
        command
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
}

#[cfg(windows)]
impl eframe::App for WindowsLauncherApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space(18.0);
            ui.heading("Clew");
            ui.add_space(8.0);
            if self.site.is_some() {
                ui.label("How should this computer participate?");
                ui.add_space(18.0);
                if ui
                    .add_sized(
                        [400.0, 64.0],
                        eframe::egui::Button::new(
                            "Use this computer\nAllow the owner-approved Clew capabilities on this machine",
                        ),
                    )
                    .clicked()
                {
                    match self.start(Some(Role::UseThisMachine)) {
                        Ok(()) => ui
                            .ctx()
                            .send_viewport_cmd(eframe::egui::ViewportCommand::Close),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                ui.add_space(10.0);
                if ui
                    .add_sized(
                        [400.0, 64.0],
                        eframe::egui::Button::new(
                            "Help nearby computers connect\nConnector only; no file or shell authority",
                        ),
                    )
                    .clicked()
                {
                    match self.start(Some(Role::ConnectorOnly)) {
                        Ok(()) => ui
                            .ctx()
                            .send_viewport_cmd(eframe::egui::ViewportCommand::Close),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
            } else {
                ui.label("Controller and agent access for this computer");
                ui.add_space(24.0);
                if ui
                    .add_sized([320.0, 56.0], eframe::egui::Button::new("Open Clew"))
                    .clicked()
                {
                    match self.start(None) {
                        Ok(()) => ui
                            .ctx()
                            .send_viewport_cmd(eframe::egui::ViewportCommand::Close),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                ui.add_space(10.0);
                ui.small(
                    "The Controller runs in the background without a console window. MCP can be started from the Clew window.",
                );
            }
            if let Some(error) = &self.error {
                ui.add_space(18.0);
                ui.label(format!("Could not start Clew: {error}"));
            }
        });
    }
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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
