#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

#[cfg(windows)]
use std::{
    fs::{OpenOptions, TryLockError},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use clew_core::StateLayout;
#[cfg(windows)]
use clew_host::SignedSiteClew;
#[cfg(windows)]
use clew_identity::ControllerIdentityStore;
#[cfg(windows)]
use clew_runtime::ControllerControlStore;
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
#[cfg(windows)]
const RELEASE_MANIFEST_FILE: &str = "release-manifest.json";
#[cfg(windows)]
const LOCAL_ACCEPTANCE_STATE_DIR: &str = "Clew-Acceptance-v1";
#[cfg(windows)]
const MAX_RELEASE_MANIFEST_BYTES: u64 = 64 * 1024;
#[cfg(windows)]
const LEGACY_ACCEPTANCE_STATE_PREFIX: &str = "clew-a-";
#[cfg(windows)]
const SUPERSEDED_ACCEPTANCE_STATE_PREFIX: &str = "Clew-Acceptance-v1.superseded-";

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
fn configure_light_theme(ctx: &eframe::egui::Context) {
    ctx.set_theme(eframe::egui::Theme::Light);
    let mut visuals = eframe::egui::Visuals::light();
    visuals.panel_fill = eframe::egui::Color32::from_rgb(246, 248, 252);
    visuals.window_fill = eframe::egui::Color32::WHITE;
    visuals.faint_bg_color = eframe::egui::Color32::from_rgb(239, 243, 249);
    visuals.selection.bg_fill = eframe::egui::Color32::from_rgb(37, 99, 235);
    visuals.selection.stroke = eframe::egui::Stroke::new(1.0, eframe::egui::Color32::WHITE);
    ctx.set_visuals_of(eframe::egui::Theme::Light, visuals);

    let mut style = (*ctx.style_of(eframe::egui::Theme::Light)).clone();
    style.spacing.item_spacing = eframe::egui::vec2(10.0, 8.0);
    style.spacing.button_padding = eframe::egui::vec2(14.0, 9.0);
    style.spacing.interact_size.y = 36.0;
    style.text_styles.insert(
        eframe::egui::TextStyle::Heading,
        eframe::egui::FontId::proportional(24.0),
    );
    style.text_styles.insert(
        eframe::egui::TextStyle::Body,
        eframe::egui::FontId::proportional(15.0),
    );
    ctx.set_style_of(eframe::egui::Theme::Light, style);
}

#[cfg(windows)]
fn launcher_mark(ui: &mut eframe::egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(eframe::egui::vec2(58.0, 58.0), eframe::egui::Sense::hover());
    let center = rect.center();
    let painter = ui.painter();
    painter.circle_filled(center, 28.0, eframe::egui::Color32::from_rgb(219, 234, 254));
    let stroke = eframe::egui::Stroke::new(2.6, eframe::egui::Color32::from_rgb(29, 78, 216));
    let nodes = [
        eframe::egui::pos2(center.x, center.y - 11.0),
        eframe::egui::pos2(center.x - 12.0, center.y + 9.0),
        eframe::egui::pos2(center.x + 12.0, center.y + 9.0),
    ];
    painter.line_segment([nodes[0], nodes[1]], stroke);
    painter.line_segment([nodes[0], nodes[2]], stroke);
    painter.line_segment([nodes[1], nodes[2]], stroke);
    for node in nodes {
        painter.circle_filled(node, 4.2, eframe::egui::Color32::WHITE);
        painter.circle_stroke(node, 4.2, stroke);
    }
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct SiteKitDisplayIdentity {
    site_name: String,
    credential_id: String,
    invite_id: String,
}

#[cfg(windows)]
fn load_site_kit_display_identity(
    path: &Path,
) -> Result<SiteKitDisplayIdentity, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    let site = SignedSiteClew::from_bytes(&bytes)?;
    site.verify()?;
    Ok(SiteKitDisplayIdentity {
        site_name: site.payload.bootstrap.payload.site_name.clone(),
        credential_id: site.site_access_credential_id(),
        invite_id: site.payload.bootstrap.payload.invite_id.to_string(),
    })
}

#[cfg(windows)]
fn local_acceptance_state_dir(root: &Path) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let manifest = root.join(RELEASE_MANIFEST_FILE);
    if !manifest.exists() {
        return Ok(None);
    }
    require_regular_file(&manifest, RELEASE_MANIFEST_FILE)?;
    let metadata = fs::metadata(&manifest)?;
    if metadata.len() > MAX_RELEASE_MANIFEST_BYTES {
        return Err("release-manifest.json is unexpectedly large".into());
    }
    let manifest_bytes = fs::read(&manifest)?;
    let value: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    if !value
        .get("dirty")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    // Dirty local acceptance builds must remain isolated from production %LOCALAPPDATA%\\Clew,
    // but the Controller identity must also survive rN -> rN+1 package upgrades. Binding state
    // to an exact extraction path/manifest silently rotated ControllerId on every local build,
    // leaving already-enrolled B/C members attached to the previous A. Keep one short,
    // versioned acceptance-only state root instead. Formal dirty=false releases still return
    // None and therefore continue to use the normal production state.
    let local_app_data = env::var_os("LOCALAPPDATA")
        .ok_or("LOCALAPPDATA is required for local Clew acceptance state")?;
    Ok(Some(
        PathBuf::from(local_app_data).join(LOCAL_ACCEPTANCE_STATE_DIR),
    ))
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcceptanceStateSummary {
    controller_id: clew_core::ControllerId,
    device_count: usize,
    generation: u64,
}

#[cfg(windows)]
fn inspect_acceptance_state(
    root: &Path,
) -> Result<Option<AcceptanceStateSummary>, Box<dyn std::error::Error>> {
    let layout = StateLayout::new(root);
    if !layout.controller_state_path().is_file() {
        return Ok(None);
    }
    if !layout.controller_control_slot_a_path().is_file()
        && !layout.controller_control_slot_b_path().is_file()
    {
        return Ok(None);
    }
    let Some(identity) = ControllerIdentityStore::new(layout.clone()).load()? else {
        return Ok(None);
    };
    let controller_id = identity.identity().controller_id();
    let control = ControllerControlStore::load_or_create(layout, controller_id)?;
    Ok(Some(AcceptanceStateSummary {
        controller_id,
        device_count: control.snapshot().catalog.devices.len(),
        generation: control.snapshot().generation,
    }))
}

#[cfg(windows)]
fn best_legacy_acceptance_state(
    temp_root: &Path,
) -> Result<Option<(PathBuf, AcceptanceStateSummary)>, Box<dyn std::error::Error>> {
    if !temp_root.is_dir() {
        return Ok(None);
    }
    let mut best: Option<(PathBuf, AcceptanceStateSummary)> = None;
    for entry in fs::read_dir(temp_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if !name
            .to_string_lossy()
            .starts_with(LEGACY_ACCEPTANCE_STATE_PREFIX)
        {
            continue;
        }
        let Ok(Some(summary)) = inspect_acceptance_state(&entry.path()) else {
            continue;
        };
        if summary.device_count == 0 {
            continue;
        }
        let replace = best.as_ref().is_none_or(|(_, current)| {
            (summary.device_count, summary.generation) > (current.device_count, current.generation)
        });
        if replace {
            best = Some((entry.path(), summary));
        }
    }
    Ok(best)
}

#[cfg(windows)]
fn controller_state_is_busy(root: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let lock_path = StateLayout::new(root).controller_lock_path();
    if !lock_path.is_file() {
        return Ok(false);
    }
    let file = OpenOptions::new().read(true).write(true).open(lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

#[cfg(windows)]
fn acceptance_copy_skips(relative: &Path) -> bool {
    let relative = relative.to_string_lossy().replace('\\', "/");
    matches!(
        relative.as_str(),
        "v1/controller.lock" | "v1/local-api.secret" | "v1/controller.sock"
    ) || relative == "v1/host-runtime"
        || relative.starts_with("v1/host-runtime/")
        || relative == "v1/client-flavors"
        || relative.starts_with("v1/client-flavors/")
}

#[cfg(windows)]
fn copy_acceptance_tree(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path.strip_prefix(source_root)?;
        if acceptance_copy_skips(relative) {
            continue;
        }
        let destination = destination_root.join(relative);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(format!(
                "legacy acceptance state contains a symbolic link: {}",
                relative.display()
            )
            .into());
        }
        if file_type.is_dir() {
            fs::create_dir_all(&destination)?;
            copy_acceptance_tree(source_root, &source_path, destination_root)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn unique_acceptance_sibling(
    stable: &Path,
    label: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let parent = stable
        .parent()
        .ok_or("acceptance state root has no parent directory")?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(parent.join(format!("{label}{}-{nonce}", std::process::id())))
}

#[cfg(windows)]
fn migrate_idle_legacy_acceptance_state(
    stable: &Path,
    legacy: &Path,
    expected: AcceptanceStateSummary,
) -> Result<(), Box<dyn std::error::Error>> {
    let staging = unique_acceptance_sibling(stable, ".Clew-Acceptance-v1.migrate-")?;
    fs::create_dir_all(&staging)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        copy_acceptance_tree(legacy, legacy, &staging)?;
        let copied = inspect_acceptance_state(&staging)?
            .ok_or("copied legacy acceptance state is incomplete")?;
        if copied != expected {
            return Err("copied legacy acceptance state changed during migration".into());
        }

        let archive = if stable.exists() {
            let archive = unique_acceptance_sibling(stable, SUPERSEDED_ACCEPTANCE_STATE_PREFIX)?;
            fs::rename(stable, &archive)?;
            Some(archive)
        } else {
            None
        };
        match fs::rename(&staging, stable) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(archive) = archive {
                    let _ = fs::rename(archive, stable);
                }
                Err(error.into())
            }
        }
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[cfg(windows)]
fn prepare_local_acceptance_state(
    stable: &Path,
    temp_root: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let stable_summary = inspect_acceptance_state(stable)?;
    if stable_summary.is_some_and(|summary| summary.device_count > 0) {
        return Ok(stable.to_path_buf());
    }
    let Some((legacy, legacy_summary)) = best_legacy_acceptance_state(temp_root)? else {
        return Ok(stable.to_path_buf());
    };

    // Never rename live Controller state underneath a running rN process. Prefer the already
    // enrolled legacy identity for this launch; a later idle launch can migrate it atomically.
    if controller_state_is_busy(stable)? || controller_state_is_busy(&legacy)? {
        return Ok(legacy);
    }

    migrate_idle_legacy_acceptance_state(stable, &legacy, legacy_summary)?;
    Ok(stable.to_path_buf())
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
    let site_identity = site
        .as_ref()
        .map(|site| load_site_kit_display_identity(site))
        .transpose()?;
    let runtime = if site.is_some() {
        root.join(".clew-runtime").join("clew.exe")
    } else {
        root.join("clew.exe")
    };
    require_regular_file(&runtime, "Clew runtime")?;
    let controller_state_dir = if site.is_none() {
        local_acceptance_state_dir(&root)?
            .map(|stable| prepare_local_acceptance_state(&stable, &env::temp_dir()))
            .transpose()?
    } else {
        None
    };

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([620.0, 480.0])
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native(
        "Clew",
        options,
        Box::new(move |cc| {
            configure_light_theme(&cc.egui_ctx);
            Ok(Box::new(WindowsLauncherApp::new(
                runtime,
                site,
                site_identity,
                controller_state_dir,
            )))
        }),
    )?;
    Ok(0)
}

#[cfg(windows)]
struct WindowsLauncherApp {
    runtime: PathBuf,
    site: Option<PathBuf>,
    site_identity: Option<SiteKitDisplayIdentity>,
    require_helper_for_target: bool,
    controller_state_dir: Option<PathBuf>,
    error: Option<String>,
}

#[cfg(windows)]
impl WindowsLauncherApp {
    fn new(
        runtime: PathBuf,
        site: Option<PathBuf>,
        site_identity: Option<SiteKitDisplayIdentity>,
        controller_state_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            runtime,
            site,
            site_identity,
            require_helper_for_target: false,
            controller_state_dir,
            error: None,
        }
    }

    fn start(&mut self, role: Option<Role>) -> Result<(), Box<dyn std::error::Error>> {
        if role == Some(Role::ConnectorOnly) {
            let status = Command::new(&self.runtime)
                .arg("windows-helper-firewall-ensure")
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(format!(
                    "Clew could not prepare Windows Firewall for helper C (exit {}). Allow the UAC request and try again.",
                    status.code().unwrap_or(-1)
                )
                .into());
            }
        }
        let mut command = Command::new(&self.runtime);
        if let Some(site) = &self.site {
            command.arg("host").arg("--site").arg(site);
            if role == Some(Role::ConnectorOnly) {
                command.arg("--connector-only");
            }
            if role == Some(Role::UseThisMachine) && self.require_helper_for_target {
                command.arg("--require-helper");
            }
        } else {
            command.arg("gui").env("CLEW_GUI_RUNTIME", "1");
            if let Some(state_dir) = &self.controller_state_dir {
                command
                    .arg("--state-dir")
                    .arg(state_dir)
                    .arg("--local-acceptance-runtime");
            }
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
    fn clear_color(&self, _visuals: &eframe::egui::Visuals) -> [f32; 4] {
        [245.0 / 255.0, 247.0 / 255.0, 250.0 / 255.0, 1.0]
    }

    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        eframe::egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(14.0);
            launcher_mark(ui);
            ui.add_space(6.0);
            ui.heading("Clew");
            ui.add_space(8.0);
            if self.site.is_some() {
                ui.label("This same Site Kit can be used for the two normal Clew setups.");
                if let Some(identity) = &self.site_identity {
                    eframe::egui::Frame::new()
                        .fill(eframe::egui::Color32::from_rgb(239, 246, 255))
                        .corner_radius(eframe::egui::CornerRadius::same(10))
                        .inner_margin(eframe::egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.strong(format!(
                                "Site Access Credential: {}",
                                identity.credential_id
                            ));
                            ui.label(format!("Site: {}", identity.site_name));
                            ui.small(format!("InviteId: {}", identity.invite_id));
                            ui.small("Controller A should show this same Credential ID. If it does not match, stop and confirm you have the intended Site Kit.");
                        });
                    ui.add_space(10.0);
                }
                ui.strong("Choose what this computer is:");
                ui.add_space(18.0);
                eframe::egui::Frame::new()
                    .fill(eframe::egui::Color32::from_rgb(248, 250, 252))
                    .corner_radius(eframe::egui::CornerRadius::same(8))
                    .inner_margin(eframe::egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.checkbox(
                            &mut self.require_helper_for_target,
                            "Require nearby helper C for target B",
                        );
                        ui.small("When enabled, B will only connect through a verified nearby helper C. Direct B-to-A dialing stays disabled even if B also has Internet access.");
                    });
                ui.add_space(10.0);
                if ui
                    .add_sized(
                        [400.0, 64.0],
                        eframe::egui::Button::new(
                            eframe::egui::RichText::new(
                                "Use this computer — B (target)\nChoose this on the collaborator computer the Controller should access",
                            )
                            .color(eframe::egui::Color32::WHITE)
                            .strong(),
                        )
                        .fill(eframe::egui::Color32::from_rgb(37, 99, 235)),
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
                            eframe::egui::RichText::new(
                                "Help nearby computers connect — C (helper)\nA/B/C only: choose this on the online computer that can reach private B",
                            )
                            .strong(),
                        )
                        .fill(eframe::egui::Color32::from_rgb(232, 240, 254)),
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
                ui.add_space(12.0);
                ui.small("A/B: A is the Controller and this computer is B. If B can reach the Internet, choose “Use this computer”.");
                ui.small("A/B/C: A is the Controller, B is the private target, and C is the online helper. On C choose “Help nearby computers connect”; Clew will request Windows Firewall permission when needed and advertise C on the local network. On B use the same Site Kit, enable the helper option, and choose “Use this computer”.");
                ui.small("If this computer is B, choose the first button. If it is C, choose the second. Helper C never exposes its own files or commands.");
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use clew_core::{
        ControllerSiteRecord, DeviceNameOrigin, DeviceRecord, InviteId, ReadPolicy, SiteId,
    };
    use clew_identity::{ControllerIdentity, DeviceIdentity, PermissionGrant, SiteBootstrapSpec};

    fn seed_acceptance_state(root: &Path, seed: u8, device_count: usize) -> AcceptanceStateSummary {
        let layout = StateLayout::new(root);
        fs::create_dir_all(layout.version_root()).unwrap();
        let controller = ControllerIdentity::from_secret([seed; 32]);
        let controller_id = controller.controller_id();
        ControllerIdentityStore::new(layout.clone())
            .restore_empty(
                ControllerIdentity::from_secret([seed; 32]),
                [seed.wrapping_add(1); 32],
            )
            .unwrap();
        let mut control =
            ControllerControlStore::load_or_create(layout.clone(), controller_id).unwrap();
        if device_count > 0 {
            let site_id = SiteId::new();
            let now = 1_000_000_u64;
            control
                .transaction(|snapshot| {
                    snapshot.catalog.upsert_site(ControllerSiteRecord {
                        site_id,
                        site_name: "Acceptance Lab".into(),
                        read_policy: ReadPolicy::all_filesystem(4_096, 2_000)?,
                        revoked: false,
                    })?;
                    for index in 0..device_count {
                        let invite_id = InviteId::new();
                        let pass = snapshot.registry.issue_bootstrap(
                            &controller,
                            SiteBootstrapSpec {
                                site_id,
                                invite_id,
                                site_name: "Acceptance Lab".into(),
                                grant: PermissionGrant::EXECUTE_READ_CONNECTOR,
                                not_before_unix_ms: now - 1,
                                expires_unix_ms: now + 60_000,
                                deployment_window_ms: 60_000,
                                max_claims: 1,
                            },
                        )?;
                        let device = DeviceIdentity::from_secret(
                            [seed.wrapping_add(10).wrapping_add(index as u8); 32],
                        );
                        let receipt =
                            snapshot
                                .registry
                                .claim(&pass, device.public_identity(), now)?;
                        snapshot.registry.finalize_host_persist(
                            invite_id,
                            receipt.device_id,
                            receipt.persist_ack_token(),
                        )?;
                        snapshot.catalog.register_device(DeviceRecord {
                            device_id: receipt.device_id,
                            site_id,
                            display_name: format!("TEST-{index}"),
                            hostname_observed: format!("TEST-{index}"),
                            capabilities: receipt.effective_grant.member,
                            enrolled_via_invite_id: invite_id,
                            name_origin: DeviceNameOrigin::Automatic {
                                base_hostname: format!("TEST-{index}"),
                                tagged: false,
                                tag_generation: 0,
                            },
                        })?;
                    }
                    Ok(())
                })
                .unwrap();
        }
        inspect_acceptance_state(root).unwrap().unwrap()
    }

    #[test]
    fn dirty_release_uses_stable_isolated_acceptance_state_across_packages() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(local_acceptance_state_dir(root.path()).unwrap(), None);

        fs::write(
            root.path().join(RELEASE_MANIFEST_FILE),
            br#"{"schema_version":2,"dirty":false}"#,
        )
        .unwrap();
        assert_eq!(local_acceptance_state_dir(root.path()).unwrap(), None);

        fs::write(
            root.path().join(RELEASE_MANIFEST_FILE),
            br#"{"schema_version":2,"dirty":true}"#,
        )
        .unwrap();
        let first = local_acceptance_state_dir(root.path()).unwrap().unwrap();
        let second = local_acceptance_state_dir(root.path()).unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some(LOCAL_ACCEPTANCE_STATE_DIR)
        );
        assert!(!first.starts_with(root.path()));
        assert!(!first.ends_with("Clew"));

        let other = tempfile::tempdir().unwrap();
        fs::write(
            other.path().join(RELEASE_MANIFEST_FILE),
            br#"{"schema_version":2,"dirty":true}"#,
        )
        .unwrap();
        assert_eq!(
            first,
            local_acceptance_state_dir(other.path()).unwrap().unwrap(),
            "local acceptance package upgrades must preserve Controller identity"
        );
    }

    #[test]
    fn idle_enrolled_legacy_state_migrates_over_empty_r7_stable_state() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join(LOCAL_ACCEPTANCE_STATE_DIR);
        let legacy = temp.path().join("clew-a-r6-enrolled");
        let empty = seed_acceptance_state(&stable, 41, 0);
        let enrolled = seed_acceptance_state(&legacy, 42, 2);
        assert_ne!(empty.controller_id, enrolled.controller_id);
        let stale_cache = legacy
            .join("v1")
            .join("client-flavors")
            .join("entries")
            .join("stale-runtime");
        fs::create_dir_all(&stale_cache).unwrap();
        fs::write(stale_cache.join("cache-entry.json"), b"stale").unwrap();

        let selected = prepare_local_acceptance_state(&stable, temp.path()).unwrap();
        assert_eq!(selected, stable);
        assert_eq!(inspect_acceptance_state(&stable).unwrap(), Some(enrolled));
        assert!(temp.path().read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(SUPERSEDED_ACCEPTANCE_STATE_PREFIX)
        }));
        assert!(!stable.join("v1").join("local-api.secret").exists());
        assert!(
            !stable.join("v1").join("client-flavors").exists(),
            "package-derived runtime cache must not migrate with Controller identity/enrollment state"
        );
    }

    #[test]
    fn enrolled_stable_state_never_regresses_to_legacy_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join(LOCAL_ACCEPTANCE_STATE_DIR);
        let legacy = temp.path().join("clew-a-r6-enrolled");
        let stable_summary = seed_acceptance_state(&stable, 51, 1);
        let _legacy_summary = seed_acceptance_state(&legacy, 52, 2);

        let selected = prepare_local_acceptance_state(&stable, temp.path()).unwrap();
        assert_eq!(selected, stable);
        assert_eq!(
            inspect_acceptance_state(&stable).unwrap(),
            Some(stable_summary)
        );
    }

    #[test]
    fn busy_legacy_controller_is_reused_without_renaming_live_state() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join(LOCAL_ACCEPTANCE_STATE_DIR);
        let legacy = temp.path().join("clew-a-r6-live");
        let _empty = seed_acceptance_state(&stable, 61, 0);
        let enrolled = seed_acceptance_state(&legacy, 62, 2);
        let lock_path = StateLayout::new(&legacy).controller_lock_path();
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.try_lock().unwrap();

        let selected = prepare_local_acceptance_state(&stable, temp.path()).unwrap();
        assert_eq!(selected, legacy);
        assert_eq!(inspect_acceptance_state(&legacy).unwrap(), Some(enrolled));
        assert_eq!(
            inspect_acceptance_state(&stable)
                .unwrap()
                .unwrap()
                .device_count,
            0
        );
    }
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
