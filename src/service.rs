use std::{error::Error, path::PathBuf};

#[cfg(target_os = "linux")]
use std::{env, fs};
#[cfg(any(target_os = "linux", test))]
use std::{ffi::OsStr, path::Path};

use clap::ValueEnum;
use serde::Serialize;

#[cfg(target_os = "linux")]
mod linux_system;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub const USER_SERVICE_UNIT: &str = "clew-controller.service";
#[cfg(any(target_os = "linux", test))]
const MANAGED_HEADER: &str = "# Managed by Clew V7a\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ServiceScope {
    User,
    Machine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ServiceAction {
    Status,
    Gui,
    Install,
    Enable,
    Start,
    Stop,
    Disable,
    Uninstall,
    EnableLinger,
    DisableLinger,
}

#[derive(Debug, Serialize)]
pub struct ServiceReport {
    pub action: String,
    pub scope: String,
    pub unit_name: String,
    pub unit_path: PathBuf,
    pub installed: bool,
    pub managed: bool,
    pub manager_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linger_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_ipc_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_executable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_connector: Option<bool>,
}

pub fn manage(
    action: ServiceAction,
    scope: ServiceScope,
    state_dir: Option<PathBuf>,
    site: Option<PathBuf>,
) -> Result<ServiceReport, Box<dyn Error>> {
    match scope {
        ServiceScope::User => {
            if site.is_some() {
                return Err(
                    "--site is only accepted by `clew service install --scope machine`".into(),
                );
            }
            if action != ServiceAction::Install && state_dir.is_some() {
                return Err(
                    "--state-dir is only accepted by `clew service install --scope user`".into(),
                );
            }
            #[cfg(target_os = "linux")]
            {
                return manage_linux_user(action, state_dir);
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (action, state_dir);
                Err("`clew service --scope user` is currently available on Linux only".into())
            }
        }
        ServiceScope::Machine => {
            if state_dir.is_some() {
                return Err("machine service state is fixed by the platform service runtime; --state-dir is not accepted".into());
            }
            if matches!(
                action,
                ServiceAction::EnableLinger | ServiceAction::DisableLinger
            ) {
                return Err(
                    "linger is a Linux user-service concept and is not available for machine scope"
                        .into(),
                );
            }
            if action != ServiceAction::Install && site.is_some() {
                return Err(
                    "--site is only accepted by `clew service install --scope machine`".into(),
                );
            }
            #[cfg(windows)]
            {
                return windows::manage_machine(action, site);
            }
            #[cfg(target_os = "linux")]
            {
                return linux_system::manage_machine(action, site);
            }
            #[cfg(not(any(windows, target_os = "linux")))]
            {
                let _ = (action, site);
                Err("`clew service --scope machine` is currently available on Windows and Linux only".into())
            }
        }
    }
}

pub async fn enrich_report(scope: ServiceScope, report: &mut ServiceReport) {
    #[cfg(windows)]
    if scope == ServiceScope::Machine {
        windows::enrich_machine_report(report).await;
    }
    #[cfg(not(windows))]
    let _ = (scope, report);
}

#[must_use]
pub fn is_windows_service_cleanup_process() -> bool {
    #[cfg(windows)]
    {
        let mut args = std::env::args_os();
        let _ = args.next();
        return args
            .next()
            .is_some_and(|arg| arg == windows::SERVICE_CLEANUP_ARGUMENT);
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn run_windows_service_cleanup_process() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        return windows::run_cleanup_process();
    }
    #[cfg(not(windows))]
    {
        Err("Windows service cleanup process mode is unavailable on this platform".into())
    }
}

#[must_use]
pub fn is_windows_service_process() -> bool {
    #[cfg(windows)]
    {
        let mut args = std::env::args_os();
        let _ = args.next();
        return args
            .next()
            .is_some_and(|arg| arg == windows::SERVICE_PROCESS_ARGUMENT);
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn run_windows_service_process() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        return windows::run_dispatcher();
    }
    #[cfg(not(windows))]
    {
        Err("Windows service process mode is unavailable on this platform".into())
    }
}

#[cfg(any(target_os = "linux", test))]
fn render_user_unit(executable: &Path, state_root: &Path) -> Result<String, Box<dyn Error>> {
    if !is_systemd_absolute_path(executable.as_os_str())?
        || !is_systemd_absolute_path(state_root.as_os_str())?
    {
        return Err(
            "systemd user service executable and state root must be Unix absolute paths".into(),
        );
    }
    let executable = systemd_quote(executable.as_os_str())?;
    let state_root = systemd_quote(state_root.as_os_str())?;
    Ok(format!(
        "{MANAGED_HEADER}[Unit]\nDescription=Clew Controller (user service)\n\n[Service]\nType=simple\nEnvironment=CLEW_CONTROLLER_LIFECYCLE=systemd-user\nExecStart={executable} controller --state-dir {state_root}\nRestart=on-failure\nRestartSec=2s\nUMask=0077\nNoNewPrivileges=true\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(any(target_os = "linux", test))]
fn is_systemd_absolute_path(value: &OsStr) -> Result<bool, Box<dyn Error>> {
    Ok(value
        .to_str()
        .ok_or("systemd unit paths must be valid UTF-8")?
        .starts_with('/'))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &OsStr) -> Result<String, Box<dyn Error>> {
    let value = value
        .to_str()
        .ok_or("systemd unit paths must be valid UTF-8")?;
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("systemd unit path is empty or contains control bytes".into());
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

#[cfg(any(target_os = "linux", test))]
fn is_managed_unit(bytes: &[u8]) -> bool {
    bytes.starts_with(MANAGED_HEADER.as_bytes())
}

#[cfg(target_os = "linux")]
fn manage_linux_user(
    action: ServiceAction,
    state_dir: Option<PathBuf>,
) -> Result<ServiceReport, Box<dyn Error>> {
    let unit_path = user_unit_path()?;
    match action {
        ServiceAction::Status => {}
        ServiceAction::Gui => {
            return Err("service GUI is not a Linux user-service lifecycle action".into());
        }
        ServiceAction::Install => install_user_unit(&unit_path, state_dir)?,
        ServiceAction::Enable => {
            require_managed_unit(&unit_path)?;
            require_user_manager()?;
            run_checked("systemctl", &["--user", "enable", USER_SERVICE_UNIT])?;
        }
        ServiceAction::Start => {
            require_managed_unit(&unit_path)?;
            require_user_manager()?;
            run_checked("systemctl", &["--user", "start", USER_SERVICE_UNIT])?;
        }
        ServiceAction::Stop => {
            require_managed_unit(&unit_path)?;
            require_user_manager()?;
            run_checked("systemctl", &["--user", "stop", USER_SERVICE_UNIT])?;
        }
        ServiceAction::Disable => {
            require_managed_unit(&unit_path)?;
            require_user_manager()?;
            run_checked("systemctl", &["--user", "disable", USER_SERVICE_UNIT])?;
        }
        ServiceAction::Uninstall => uninstall_user_unit(&unit_path)?,
        ServiceAction::EnableLinger => set_linger(true)?,
        ServiceAction::DisableLinger => set_linger(false)?,
    }
    service_report(action, unit_path)
}

#[cfg(target_os = "linux")]
fn install_user_unit(unit_path: &Path, state_dir: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    require_user_manager()?;
    let previous = if unit_path.exists() {
        let existing = fs::read(unit_path)?;
        if !is_managed_unit(&existing) {
            return Err(format!(
                "refusing to overwrite an unmanaged systemd unit: {}",
                unit_path.display()
            )
            .into());
        }
        Some(existing)
    } else {
        None
    };
    let executable = fs::canonicalize(env::current_exe()?)?;
    let state_root = match state_dir {
        Some(path) => absolute_path(path)?,
        None => clew_runtime::ControllerConfig::for_current_user()?
            .state_root()
            .to_path_buf(),
    };
    let unit = render_user_unit(&executable, &state_root)?;
    let parent = unit_path
        .parent()
        .ok_or("systemd user unit path has no parent")?;
    fs::create_dir_all(parent)?;
    atomic_write_unit(unit_path, unit.as_bytes())?;
    if let Err(error) = run_checked("systemctl", &["--user", "daemon-reload"]) {
        let rollback = match previous {
            Some(bytes) => atomic_write_unit(unit_path, &bytes),
            None => (|| -> Result<(), Box<dyn Error>> {
                fs::remove_file(unit_path)?;
                sync_parent(unit_path)?;
                Ok(())
            })(),
        };
        let rollback_reload = run_checked("systemctl", &["--user", "daemon-reload"]);
        if let Err(rollback_error) = rollback {
            return Err(format!(
                "{error}; restoring the previous Clew user unit also failed: {rollback_error}"
            )
            .into());
        }
        if let Err(rollback_reload_error) = rollback_reload {
            return Err(format!(
                "{error}; the unit file was restored but systemd reload also failed: {rollback_reload_error}"
            )
            .into());
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_user_unit(unit_path: &Path) -> Result<(), Box<dyn Error>> {
    require_user_manager()?;
    if !unit_path.exists() {
        return Ok(());
    }
    require_managed_unit(unit_path)?;
    let active = query_systemctl(&["--user", "is-active", USER_SERVICE_UNIT])
        .is_some_and(|state| state == "active" || state == "activating" || state == "reloading");
    if active {
        run_checked("systemctl", &["--user", "stop", USER_SERVICE_UNIT])?;
    }
    let enabled = query_systemctl(&["--user", "is-enabled", USER_SERVICE_UNIT])
        .is_some_and(|state| state == "enabled" || state == "enabled-runtime" || state == "linked");
    if enabled {
        run_checked("systemctl", &["--user", "disable", USER_SERVICE_UNIT])?;
    }
    fs::remove_file(unit_path)?;
    sync_parent(unit_path)?;
    run_checked("systemctl", &["--user", "daemon-reload"])?;
    let _ = run_checked("systemctl", &["--user", "reset-failed", USER_SERVICE_UNIT]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn service_report(
    action: ServiceAction,
    unit_path: PathBuf,
) -> Result<ServiceReport, Box<dyn Error>> {
    let installed = unit_path.is_file();
    let managed = installed && fs::read(&unit_path).is_ok_and(|bytes| is_managed_unit(&bytes));
    let manager_available = user_manager_available();
    let (enable_state, active_state) = if manager_available {
        (
            query_systemctl(&["--user", "is-enabled", USER_SERVICE_UNIT]),
            query_systemctl(&["--user", "is-active", USER_SERVICE_UNIT]),
        )
    } else {
        (None, None)
    };
    Ok(ServiceReport {
        action: action_name(action).into(),
        scope: "user".into(),
        unit_name: USER_SERVICE_UNIT.into(),
        unit_path,
        installed,
        managed,
        manager_available,
        enable_state,
        active_state,
        linger_enabled: query_linger(),
        process_id: None,
        control_ipc_available: None,
        runtime_state: None,
        runtime_site_name: None,
        runtime_device_id: None,
        runtime_executable: None,
        runtime_connector: None,
    })
}

#[cfg(target_os = "linux")]
fn user_unit_path() -> Result<PathBuf, Box<dyn Error>> {
    let config_root = match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("XDG_CONFIG_HOME must be absolute for service installation".into());
            }
            path
        }
        _ => {
            let home = PathBuf::from(env::var_os("HOME").ok_or("HOME is unavailable")?);
            if !home.is_absolute() {
                return Err("HOME must be absolute for service installation".into());
            }
            home.join(".config")
        }
    };
    Ok(config_root
        .join("systemd")
        .join("user")
        .join(USER_SERVICE_UNIT))
}

#[cfg(target_os = "linux")]
fn absolute_path(path: PathBuf) -> Result<PathBuf, Box<dyn Error>> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

#[cfg(target_os = "linux")]
fn require_managed_unit(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.is_file() {
        return Err(format!("Clew user service is not installed: {}", path.display()).into());
    }
    let bytes = fs::read(path)?;
    if !is_managed_unit(&bytes) {
        return Err(format!(
            "refusing to manage an unowned systemd unit: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_user_manager() -> Result<(), Box<dyn Error>> {
    if user_manager_available() {
        Ok(())
    } else {
        Err("systemd user manager is unavailable; log in with a systemd user session before managing the service".into())
    }
}

#[cfg(target_os = "linux")]
fn user_manager_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn query_systemctl(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    let state = stdout.trim();
    if state.is_empty() {
        None
    } else {
        Some(state.to_owned())
    }
}

#[cfg(target_os = "linux")]
fn set_linger(enabled: bool) -> Result<(), Box<dyn Error>> {
    let uid = current_uid()?;
    let action = if enabled {
        "enable-linger"
    } else {
        "disable-linger"
    };
    run_checked("loginctl", &[action, &uid])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn query_linger() -> Option<bool> {
    let uid = current_uid().ok()?;
    let output = std::process::Command::new("loginctl")
        .args(["show-user", &uid, "-p", "Linger", "--value"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8(output.stdout).ok()?.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn current_uid() -> Result<String, Box<dyn Error>> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err("`id -u` failed while resolving the current user".into());
    }
    let uid = String::from_utf8(output.stdout)?.trim().to_owned();
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("`id -u` returned an invalid uid".into());
    }
    Ok(uid)
}

#[cfg(target_os = "linux")]
fn run_checked(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(format!("{program} failed with {}: {detail}", output.status).into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn atomic_write_unit(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    let parent = path.parent().ok_or("unit path has no parent")?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = parent.join(format!(
        ".{USER_SERVICE_UNIT}.tmp-{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        fs::rename(&temp, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(target_os = "linux")]
fn sync_parent(path: &Path) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("unit path has no parent")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn action_name(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Status => "status",
        ServiceAction::Gui => "gui",
        ServiceAction::Install => "install",
        ServiceAction::Enable => "enable",
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
        ServiceAction::Disable => "disable",
        ServiceAction::Uninstall => "uninstall",
        ServiceAction::EnableLinger => "enable-linger",
        ServiceAction::DisableLinger => "disable-linger",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_is_explicit_foreground_user_controller() {
        let unit = render_user_unit(
            Path::new("/opt/Clew App/clew%$\"bin"),
            Path::new("/home/alice/Clew State/%controller"),
        )
        .unwrap();
        assert!(unit.starts_with(MANAGED_HEADER));
        assert!(unit.contains("Type=simple"));
        assert!(unit.contains("Environment=CLEW_CONTROLLER_LIFECYCLE=systemd-user"));
        assert!(unit.contains("controller --state-dir"));
        assert!(!unit.contains("--systemd-user-service"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("UMask=0077"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("WantedBy=multi-user.target"));
        assert!(!unit.contains("loginctl"));
        assert!(unit.contains("%%controller"));
        assert!(unit.contains("Clew State"));
        assert!(unit.contains("%%$$\\\"bin"));
    }

    #[test]
    fn systemd_quote_rejects_control_bytes_and_relative_unit_inputs() {
        assert!(systemd_quote(OsStr::new("line\nbreak")).is_err());
        assert!(render_user_unit(Path::new("clew"), Path::new("/state")).is_err());
        assert!(render_user_unit(Path::new("/bin/clew"), Path::new("state")).is_err());
    }

    #[test]
    fn only_clew_owned_units_are_recognized() {
        assert!(is_managed_unit(
            format!("{MANAGED_HEADER}[Unit]\n").as_bytes()
        ));
        assert!(!is_managed_unit(b"[Unit]\nDescription=other\n"));
    }
}
