use std::{
    collections::BTreeSet,
    error::Error,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use clew_host::{
    LEGACY_NEARBY_CONNECTOR_FILE_NAME, NEARBY_CONNECTOR_FILE_NAME, SignedSiteClew, TargetPlatform,
    verify_outfit_asset_bytes,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ServiceAction, ServiceReport};

pub const SYSTEM_SERVICE_UNIT: &str = "clew-connector.service";
const SERVICE_ACCOUNT: &str = "clew-service";
const SERVICE_SCHEMA_VERSION: u32 = 1;
const SERVICE_ROLE: &str = "connector_only";
const MANAGED_HEADER: &str = "# Managed by Clew V7c\n";
const SYSUSERS_FILE: &str = "/etc/sysusers.d/clew-service.conf";
const UNIT_FILE: &str = "/etc/systemd/system/clew-connector.service";
const INSTALL_ROOT: &str = "/usr/local/lib/clew-service";
const INSTALLED_BINARY: &str = "/usr/local/lib/clew-service/clew";
const MACHINE_ROOT: &str = "/var/lib/clew-service";
const KIT_ROOT: &str = "/var/lib/clew-service/kit";
const SITE_FILE: &str = "/var/lib/clew-service/kit/site.clew";
const STATE_ROOT: &str = "/var/lib/clew-service/state";
const CONFIG_FILE: &str = "/var/lib/clew-service/service.json";
const MAX_NEARBY_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_SYSTEM_ID_MIN: u32 = 1;
const DEFAULT_SYSTEM_ID_MAX: u32 = 999;
const MAX_SYSTEM_ID_SCAN: u32 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ServiceIdentity {
    uid: u32,
    gid: u32,
}

impl ServiceIdentity {
    fn validate(self) -> Result<Self, Box<dyn Error>> {
        if self.uid == 0 || self.gid == 0 || self.uid != self.gid {
            return Err(
                "Linux machine service identity must use one non-root dedicated UID/GID".into(),
            );
        }
        Ok(self)
    }
}

#[derive(Clone, Debug)]
struct MachinePaths {
    unit: PathBuf,
    sysusers: PathBuf,
    install_root: PathBuf,
    binary: PathBuf,
    machine_root: PathBuf,
    kit_root: PathBuf,
    site_file: PathBuf,
    state_root: PathBuf,
    config_file: PathBuf,
}

impl MachinePaths {
    fn fixed() -> Self {
        Self {
            unit: PathBuf::from(UNIT_FILE),
            sysusers: PathBuf::from(SYSUSERS_FILE),
            install_root: PathBuf::from(INSTALL_ROOT),
            binary: PathBuf::from(INSTALLED_BINARY),
            machine_root: PathBuf::from(MACHINE_ROOT),
            kit_root: PathBuf::from(KIT_ROOT),
            site_file: PathBuf::from(SITE_FILE),
            state_root: PathBuf::from(STATE_ROOT),
            config_file: PathBuf::from(CONFIG_FILE),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct MachineServiceConfig {
    schema_version: u32,
    unit_name: String,
    service_account: String,
    role: String,
    installed_version: String,
    service_uid: u32,
    service_gid: u32,
    binary_sha256: String,
    site_sha256: String,
}

impl MachineServiceConfig {
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.schema_version != SERVICE_SCHEMA_VERSION
            || self.unit_name != SYSTEM_SERVICE_UNIT
            || self.service_account != SERVICE_ACCOUNT
            || self.role != SERVICE_ROLE
            || self.installed_version.is_empty()
            || self.service_uid == 0
            || self.service_gid == 0
            || self.service_uid != self.service_gid
            || !is_sha256(&self.binary_sha256)
            || !is_sha256(&self.site_sha256)
        {
            return Err("Linux machine service metadata is invalid or incompatible".into());
        }
        Ok(())
    }
}

pub fn manage_machine(
    action: ServiceAction,
    site: Option<PathBuf>,
) -> Result<ServiceReport, Box<dyn Error>> {
    match action {
        ServiceAction::Status => {}
        ServiceAction::Gui => {
            return Err(
                "Linux machine service has no tray/GUI client; use the explicit CLI lifecycle"
                    .into(),
            );
        }
        ServiceAction::Install => {
            let site = site.ok_or("Linux machine service install requires --site FILE")?;
            require_root()?;
            install_machine(&site)?;
        }
        ServiceAction::Enable => {
            require_root()?;
            require_machine_payload()?;
            run_checked("systemctl", &["enable", SYSTEM_SERVICE_UNIT])?;
        }
        ServiceAction::Start => {
            require_root()?;
            require_machine_payload()?;
            run_checked("systemctl", &["start", SYSTEM_SERVICE_UNIT])?;
        }
        ServiceAction::Stop => {
            require_root()?;
            require_managed_system_files()?;
            if machine_active() {
                run_checked("systemctl", &["stop", SYSTEM_SERVICE_UNIT])?;
            }
        }
        ServiceAction::Disable => {
            require_root()?;
            require_managed_system_files()?;
            if machine_enabled() {
                run_checked("systemctl", &["disable", SYSTEM_SERVICE_UNIT])?;
            }
        }
        ServiceAction::Uninstall => {
            require_root()?;
            uninstall_machine()?;
        }
        ServiceAction::EnableLinger | ServiceAction::DisableLinger => {
            return Err(
                "linger is a Linux user-service concept and is unavailable for machine scope"
                    .into(),
            );
        }
    }
    machine_report(action)
}

fn install_machine(source_site: &Path) -> Result<(), Box<dyn Error>> {
    let paths = MachinePaths::fixed();
    require_system_manager()?;
    for path in [
        &paths.unit,
        &paths.sysusers,
        &paths.install_root,
        &paths.machine_root,
    ] {
        if path.exists() {
            return Err(format!(
                "Linux machine service path already exists: {}; inspect or uninstall it before install",
                path.display()
            )
            .into());
        }
    }
    if account_exists()? || group_exists()? {
        return Err(format!(
            "Linux service identity {SERVICE_ACCOUNT} already exists as a user or group; refusing to adopt unmanaged identity state"
        )
        .into());
    }

    let source_site = fs::canonicalize(source_site)?;
    let site = SignedSiteClew::read(&source_site)?;
    let _controller = site.verify()?;
    verify_site_runtime_target(&site)?;
    if !site.payload.bootstrap.payload.grant.member.connector {
        return Err(
            "Site Kit does not grant Connector capability required by the machine service".into(),
        );
    }
    // Copy the inode that is actually executing under sudo rather than reopening a user-writable
    // argv/current_exe path that could be replaced during installation.
    let source_binary = PathBuf::from("/proc/self/exe");

    let identity = allocate_service_identity()?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        write_atomic(&paths.sysusers, render_sysusers(identity).as_bytes(), 0o644)?;
        run_checked("systemd-sysusers", &[SYSUSERS_FILE])?;
        verify_service_account(identity.uid, identity.gid)?;
        let uid = identity.uid;
        let gid = identity.gid;

        fs::create_dir_all(&paths.install_root)?;
        fs::set_permissions(&paths.install_root, fs::Permissions::from_mode(0o755))?;
        copy_binary(&source_binary, &paths.binary)?;

        create_dir_with_mode(&paths.machine_root, 0o750)?;
        create_dir_with_mode(&paths.kit_root, 0o750)?;
        create_dir_with_mode(&paths.state_root, 0o700)?;
        copy_site_kit(&source_site, &site, &paths.kit_root)?;
        let config = MachineServiceConfig {
            schema_version: SERVICE_SCHEMA_VERSION,
            unit_name: SYSTEM_SERVICE_UNIT.into(),
            service_account: SERVICE_ACCOUNT.into(),
            role: SERVICE_ROLE.into(),
            installed_version: env!("CARGO_PKG_VERSION").into(),
            service_uid: uid,
            service_gid: gid,
            binary_sha256: sha256_file(&paths.binary)?,
            site_sha256: sha256_file(&paths.site_file)?,
        };
        write_atomic(
            &paths.config_file,
            &serde_json::to_vec_pretty(&config)?,
            0o640,
        )?;
        protect_machine_tree(uid, gid, &paths)?;
        verify_machine_tree_modes(&paths, uid, gid)?;

        write_atomic(&paths.unit, render_system_unit().as_bytes(), 0o644)?;
        run_checked("systemd-analyze", &["verify", UNIT_FILE])?;
        run_checked("systemctl", &["daemon-reload"])?;
        Ok(())
    })();

    if let Err(error) = result {
        let cleanup = cleanup_failed_install(&paths, identity);
        if let Err(cleanup_error) = cleanup {
            return Err(format!(
                "{error}; Linux machine service install rollback also failed: {cleanup_error}"
            )
            .into());
        }
        return Err(error);
    }
    Ok(())
}

fn verify_site_runtime_target(site: &SignedSiteClew) -> Result<(), Box<dyn Error>> {
    let flavor = &site.payload.client_flavor;
    if flavor.platform != TargetPlatform::current()
        || flavor.arch != std::env::consts::ARCH
        || flavor.runtime_version != env!("CARGO_PKG_VERSION")
    {
        return Err(format!(
            "Site Kit targets {}/{} runtime {}, but this Clew runtime is {}/{} {}",
            flavor.platform.label(),
            flavor.arch,
            flavor.runtime_version,
            TargetPlatform::current().label(),
            std::env::consts::ARCH,
            env!("CARGO_PKG_VERSION")
        )
        .into());
    }
    Ok(())
}

fn uninstall_machine() -> Result<(), Box<dyn Error>> {
    let paths = MachinePaths::fixed();
    if !paths.unit.exists() {
        return Ok(());
    }
    let identity = require_managed_system_files()?;
    verify_service_account(identity.uid, identity.gid)?;
    if machine_active() {
        run_checked("systemctl", &["stop", SYSTEM_SERVICE_UNIT])?;
    }
    if machine_enabled() {
        run_checked("systemctl", &["disable", SYSTEM_SERVICE_UNIT])?;
    }

    // Keep the root-owned unit and sysusers credential until the dedicated identity is safely
    // removed. If local account state was unexpectedly reused, fail closed instead of deleting
    // an unrelated system identity.
    delete_service_identity_if_present(identity)?;
    remove_tree_if_exists(&paths.machine_root)?;
    remove_tree_if_exists(&paths.install_root)?;
    fs::remove_file(&paths.unit)?;
    sync_parent(&paths.unit)?;
    fs::remove_file(&paths.sysusers)?;
    sync_parent(&paths.sysusers)?;
    run_checked("systemctl", &["daemon-reload"])?;
    let _ = run_checked("systemctl", &["reset-failed", SYSTEM_SERVICE_UNIT]);
    Ok(())
}

fn cleanup_failed_install(
    paths: &MachinePaths,
    identity: ServiceIdentity,
) -> Result<(), Box<dyn Error>> {
    // The chosen UID/GID is known even if systemd-sysusers failed part-way through. Prove any
    // identity that appeared still matches that exact allocation before removing it.
    delete_service_identity_if_present(identity)?;
    if paths.unit.exists() {
        fs::remove_file(&paths.unit)?;
        sync_parent(&paths.unit)?;
    }
    let _ = run_checked("systemctl", &["daemon-reload"]);
    remove_tree_if_exists(&paths.machine_root)?;
    remove_tree_if_exists(&paths.install_root)?;
    if paths.sysusers.exists() {
        fs::remove_file(&paths.sysusers)?;
        sync_parent(&paths.sysusers)?;
    }
    Ok(())
}

fn machine_report(action: ServiceAction) -> Result<ServiceReport, Box<dyn Error>> {
    let paths = MachinePaths::fixed();
    let installed = paths.unit.is_file();
    let managed = installed && managed_system_files().unwrap_or(false);
    let manager_available = system_manager_available();
    let (enable_state, active_state, process_id) = if manager_available {
        (
            query_systemctl(&["is-enabled", SYSTEM_SERVICE_UNIT]),
            query_systemctl(&["is-active", SYSTEM_SERVICE_UNIT]),
            query_systemctl(&["show", SYSTEM_SERVICE_UNIT, "--property=MainPID", "--value"])
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|pid| *pid != 0),
        )
    } else {
        (None, None, None)
    };
    Ok(ServiceReport {
        action: action_name(action).into(),
        scope: "machine".into(),
        unit_name: SYSTEM_SERVICE_UNIT.into(),
        unit_path: paths.unit,
        installed,
        managed,
        manager_available,
        enable_state,
        active_state,
        linger_enabled: None,
        process_id,
        control_ipc_available: None,
        runtime_state: None,
        runtime_site_name: None,
        runtime_device_id: None,
        runtime_executable: None,
        runtime_connector: None,
    })
}

fn require_machine_payload() -> Result<MachinePaths, Box<dyn Error>> {
    let paths = MachinePaths::fixed();
    let identity = require_managed_system_files()?;
    let config = load_config(&paths)?;
    config.validate()?;
    if config.service_uid != identity.uid || config.service_gid != identity.gid {
        return Err(
            "Linux machine service metadata does not match its sysusers identity credential".into(),
        );
    }
    verify_service_account(config.service_uid, config.service_gid)?;
    verify_machine_tree_modes(&paths, config.service_uid, config.service_gid)?;
    if !paths.binary.is_file() || !paths.site_file.is_file() || !paths.state_root.is_dir() {
        return Err("Linux machine service installation is incomplete".into());
    }
    if sha256_file(&paths.binary)? != config.binary_sha256
        || sha256_file(&paths.site_file)? != config.site_sha256
    {
        return Err(
            "Linux machine service binary or Site Kit no longer matches installed metadata".into(),
        );
    }
    Ok(paths)
}

fn require_managed_system_files() -> Result<ServiceIdentity, Box<dyn Error>> {
    managed_service_identity()?.ok_or_else(|| {
        "refusing to manage Linux system service files that are not exactly Clew-owned".into()
    })
}

fn managed_system_files() -> Result<bool, Box<dyn Error>> {
    Ok(managed_service_identity()?.is_some())
}

fn managed_service_identity() -> Result<Option<ServiceIdentity>, Box<dyn Error>> {
    let paths = MachinePaths::fixed();
    if !paths.unit.is_file() || !paths.sysusers.is_file() {
        return Ok(None);
    }
    if fs::read(&paths.unit)? != render_system_unit().as_bytes() {
        return Ok(None);
    }
    match parse_managed_sysusers(&fs::read(&paths.sysusers)?) {
        Ok(identity) => Ok(Some(identity)),
        Err(_) => Ok(None),
    }
}

fn render_system_unit() -> String {
    format!(
        "{MANAGED_HEADER}[Unit]\nDescription=Clew Connector (system service)\nWants=network-online.target\nAfter=network-online.target\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nType=simple\nUser={SERVICE_ACCOUNT}\nGroup={SERVICE_ACCOUNT}\nEnvironment=HOME={STATE_ROOT}\nWorkingDirectory={STATE_ROOT}\nExecStart={INSTALLED_BINARY} host --site {SITE_FILE} --state-dir {STATE_ROOT} --foreground --connector-only\nRestart=on-failure\nRestartSec=2s\nKillSignal=SIGTERM\nTimeoutStopSec=20s\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\nPrivateDevices=true\nProtectSystem=strict\nProtectHome=true\nReadWritePaths={STATE_ROOT}\nProtectKernelTunables=true\nProtectKernelModules=true\nProtectControlGroups=true\nRestrictSUIDSGID=true\nLockPersonality=true\nCapabilityBoundingSet=\nAmbientCapabilities=\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn render_sysusers(identity: ServiceIdentity) -> String {
    format!(
        "{MANAGED_HEADER}g {SERVICE_ACCOUNT} {}\nu {SERVICE_ACCOUNT} {}:{} \"Clew Connector Service\" {MACHINE_ROOT}\n",
        identity.gid, identity.uid, identity.gid
    )
}

fn parse_managed_sysusers(bytes: &[u8]) -> Result<ServiceIdentity, Box<dyn Error>> {
    let text = std::str::from_utf8(bytes)?;
    if !text.ends_with('\n') {
        return Err("Clew sysusers credential must end with a newline".into());
    }
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != 3 || lines[0] != MANAGED_HEADER.trim_end_matches('\n') {
        return Err("Clew sysusers credential has an unexpected shape".into());
    }
    let group_prefix = format!("g {SERVICE_ACCOUNT} ");
    let gid = lines[1]
        .strip_prefix(&group_prefix)
        .ok_or("Clew sysusers group line is invalid")?
        .parse::<u32>()?;
    let user_prefix = format!("u {SERVICE_ACCOUNT} ");
    let user = lines[2]
        .strip_prefix(&user_prefix)
        .ok_or("Clew sysusers user line is invalid")?;
    let (ids, suffix) = user
        .split_once(' ')
        .ok_or("Clew sysusers user line is incomplete")?;
    if suffix != format!("\"Clew Connector Service\" {MACHINE_ROOT}") {
        return Err("Clew sysusers user metadata is invalid".into());
    }
    let (uid, user_gid) = ids
        .split_once(':')
        .ok_or("Clew sysusers UID:GID is invalid")?;
    let identity = ServiceIdentity {
        uid: uid.parse()?,
        gid: user_gid.parse()?,
    }
    .validate()?;
    if identity.gid != gid || render_sysusers(identity).as_bytes() != bytes {
        return Err("Clew sysusers identity credential is inconsistent".into());
    }
    Ok(identity)
}

fn load_config(paths: &MachinePaths) -> Result<MachineServiceConfig, Box<dyn Error>> {
    let bytes = fs::read(&paths.config_file)?;
    if bytes.len() > 64 * 1024 {
        return Err("Linux machine service metadata is too large".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn require_root() -> Result<(), Box<dyn Error>> {
    let uid = command_text("id", &["-u"])?;
    if uid == "0" {
        Ok(())
    } else {
        Err("Linux machine service lifecycle changes require root; re-run this explicit action through sudo".into())
    }
}

fn require_system_manager() -> Result<(), Box<dyn Error>> {
    if system_manager_available() {
        Ok(())
    } else {
        Err("systemd system manager is unavailable".into())
    }
}

fn system_manager_available() -> bool {
    Command::new("systemctl")
        .arg("show-environment")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn machine_active() -> bool {
    query_systemctl(&["is-active", SYSTEM_SERVICE_UNIT])
        .is_some_and(|state| state == "active" || state == "activating" || state == "reloading")
}

fn machine_enabled() -> bool {
    query_systemctl(&["is-enabled", SYSTEM_SERVICE_UNIT])
        .is_some_and(|state| state == "enabled" || state == "enabled-runtime" || state == "linked")
}

fn query_systemctl(args: &[&str]) -> Option<String> {
    let output = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    let value = stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn account_exists() -> Result<bool, Box<dyn Error>> {
    Ok(Command::new("getent")
        .args(["passwd", SERVICE_ACCOUNT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

fn group_exists() -> Result<bool, Box<dyn Error>> {
    Ok(Command::new("getent")
        .args(["group", SERVICE_ACCOUNT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

fn allocate_service_identity() -> Result<ServiceIdentity, Box<dyn Error>> {
    let passwd = command_text("getent", &["passwd"])?;
    let group = command_text("getent", &["group"])?;
    let login_defs = match fs::read_to_string("/etc/login.defs") {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let (min_id, max_id) = system_id_range(&login_defs)?;
    allocate_service_identity_from(&passwd, &group, min_id, max_id)
}

fn system_id_range(login_defs: &str) -> Result<(u32, u32), Box<dyn Error>> {
    let mut uid_min = None;
    let mut uid_max = None;
    let mut gid_min = None;
    let mut gid_max = None;
    for line in login_defs.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next() else {
            continue;
        };
        let target = match key {
            "SYS_UID_MIN" => &mut uid_min,
            "SYS_UID_MAX" => &mut uid_max,
            "SYS_GID_MIN" => &mut gid_min,
            "SYS_GID_MAX" => &mut gid_max,
            _ => continue,
        };
        *target = Some(value.parse::<u32>()?);
    }
    let min_id = uid_min
        .unwrap_or(DEFAULT_SYSTEM_ID_MIN)
        .max(gid_min.unwrap_or(DEFAULT_SYSTEM_ID_MIN))
        .max(1);
    let max_id = uid_max
        .unwrap_or(DEFAULT_SYSTEM_ID_MAX)
        .min(gid_max.unwrap_or(DEFAULT_SYSTEM_ID_MAX));
    if min_id > max_id || max_id.saturating_sub(min_id) >= MAX_SYSTEM_ID_SCAN {
        return Err(
            "Linux system UID/GID allocation range is invalid or unreasonably large".into(),
        );
    }
    Ok((min_id, max_id))
}

fn allocate_service_identity_from(
    passwd: &str,
    group: &str,
    min_id: u32,
    max_id: u32,
) -> Result<ServiceIdentity, Box<dyn Error>> {
    if min_id == 0 || min_id > max_id || max_id.saturating_sub(min_id) >= MAX_SYSTEM_ID_SCAN {
        return Err("Linux service identity allocation range is invalid".into());
    }
    let mut occupied = BTreeSet::new();
    for line in passwd.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() < 4 || fields[0].is_empty() {
            return Err("getent passwd returned a malformed entry".into());
        }
        occupied.insert(fields[2].parse::<u32>()?);
        // A primary GID is occupied even if /etc/group has no matching entry. This is the
        // critical case systemd-sysusers' automatic allocator can otherwise miss.
        occupied.insert(fields[3].parse::<u32>()?);
    }
    for line in group.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() < 3 || fields[0].is_empty() {
            return Err("getent group returned a malformed entry".into());
        }
        occupied.insert(fields[2].parse::<u32>()?);
    }
    for id in (min_id..=max_id).rev() {
        if !occupied.contains(&id) {
            return ServiceIdentity { uid: id, gid: id }.validate();
        }
    }
    Err("no free dedicated system UID/GID remains for the Clew machine service".into())
}

fn verify_service_account(expected_uid: u32, expected_gid: u32) -> Result<(), Box<dyn Error>> {
    let output = Command::new("getent")
        .args(["passwd", SERVICE_ACCOUNT])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err("Linux machine service account is missing".into());
    }
    let text = String::from_utf8(output.stdout)?;
    let fields = text.trim().split(':').collect::<Vec<_>>();
    if fields.len() < 7 {
        return Err("Linux machine service passwd entry is malformed".into());
    }
    let uid = fields[2].parse::<u32>()?;
    let gid = fields[3].parse::<u32>()?;
    let shell_name = Path::new(fields[6])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if uid != expected_uid
        || gid != expected_gid
        || uid == 0
        || fields[5] != MACHINE_ROOT
        || !matches!(shell_name, "nologin" | "false")
    {
        return Err("Linux machine service account no longer matches installed identity".into());
    }
    verify_service_group(expected_gid)?;
    Ok(())
}

fn verify_service_group(expected_gid: u32) -> Result<(), Box<dyn Error>> {
    let group = Command::new("getent")
        .args(["group", SERVICE_ACCOUNT])
        .stdin(Stdio::null())
        .output()?;
    if !group.status.success() {
        return Err("Linux machine service group is missing".into());
    }
    let group_text = String::from_utf8(group.stdout)?;
    let group_fields = group_text.trim().split(':').collect::<Vec<_>>();
    if group_fields.len() < 4
        || group_fields[2].parse::<u32>()? != expected_gid
        || !group_fields[3].is_empty()
    {
        return Err("Linux machine service group no longer matches its dedicated identity".into());
    }
    ensure_no_other_primary_gid_users(expected_gid)?;
    Ok(())
}

fn ensure_no_other_primary_gid_users(expected_gid: u32) -> Result<(), Box<dyn Error>> {
    let passwd = command_text("getent", &["passwd"])?;
    let mut conflicts = Vec::new();
    for line in passwd.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() < 4 || fields[0].is_empty() {
            return Err("getent passwd returned a malformed entry".into());
        }
        if fields[3].parse::<u32>()? == expected_gid && fields[0] != SERVICE_ACCOUNT {
            conflicts.push(fields[0].to_owned());
        }
    }
    if !conflicts.is_empty() {
        return Err(format!(
            "Linux machine service GID {expected_gid} is also a primary group for: {}",
            conflicts.join(", ")
        )
        .into());
    }
    Ok(())
}

fn delete_service_account() -> Result<(), Box<dyn Error>> {
    let userdel = first_existing_program(&["/usr/sbin/userdel", "/sbin/userdel"])
        .ok_or("userdel is unavailable while removing the Clew service account")?;
    let status = Command::new(userdel)
        .arg(SERVICE_ACCOUNT)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("userdel failed with {status}").into());
    }
    Ok(())
}

fn delete_service_group() -> Result<(), Box<dyn Error>> {
    let groupdel = first_existing_program(&["/usr/sbin/groupdel", "/sbin/groupdel"])
        .ok_or("groupdel is unavailable while removing the Clew service group")?;
    let status = Command::new(groupdel)
        .arg(SERVICE_ACCOUNT)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("groupdel failed with {status}").into());
    }
    Ok(())
}

fn delete_service_identity_if_present(identity: ServiceIdentity) -> Result<(), Box<dyn Error>> {
    let identity = identity.validate()?;
    if account_exists()? {
        verify_service_account(identity.uid, identity.gid)?;
    } else if group_exists()? {
        verify_service_group(identity.gid)?;
    }

    if account_exists()? {
        delete_service_account()?;
        if account_exists()? {
            return Err("Linux machine service account still exists after userdel".into());
        }
    }
    // userdel may remove a same-name private group automatically. If it does not, prove the
    // remaining group is still the exact dedicated GID and has no other primary/supplementary
    // users before deleting it.
    if group_exists()? {
        verify_service_group(identity.gid)?;
        delete_service_group()?;
        if group_exists()? {
            return Err("Linux machine service group still exists after groupdel".into());
        }
    }
    Ok(())
}

fn protect_machine_tree(uid: u32, gid: u32, paths: &MachinePaths) -> Result<(), Box<dyn Error>> {
    let chown = first_existing_program(&["/usr/bin/chown", "/bin/chown"])
        .ok_or("chown is unavailable while protecting Linux machine state")?;
    let root_group = format!("0:{gid}");
    let status = Command::new(&chown)
        .args(["-R", &root_group])
        .arg(&paths.machine_root)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("chown root:service-group failed with {status}").into());
    }
    let service_owner = format!("{uid}:{gid}");
    let status = Command::new(&chown)
        .args(["-R", &service_owner])
        .arg(&paths.state_root)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("chown service state failed with {status}").into());
    }
    fs::set_permissions(&paths.machine_root, fs::Permissions::from_mode(0o750))?;
    fs::set_permissions(&paths.kit_root, fs::Permissions::from_mode(0o750))?;
    fs::set_permissions(&paths.state_root, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&paths.site_file, fs::Permissions::from_mode(0o640))?;
    fs::set_permissions(&paths.config_file, fs::Permissions::from_mode(0o640))?;
    normalize_readonly_kit_modes(&paths.kit_root)?;
    Ok(())
}

fn normalize_readonly_kit_modes(root: &Path) -> Result<(), Box<dyn Error>> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0_usize;
    while let Some(path) = pending.pop() {
        seen = seen.saturating_add(1);
        if seen > 1024 {
            return Err("Linux machine Site Kit tree exceeds the 1024-entry safety bound".into());
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Linux machine Site Kit contains a symlink: {}",
                path.display()
            )
            .into());
        }
        if metadata.is_dir() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o750))?;
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        } else {
            return Err(format!(
                "Linux machine Site Kit contains an unsupported file type: {}",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn verify_machine_tree_modes(
    paths: &MachinePaths,
    uid: u32,
    gid: u32,
) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;

    for path in [&paths.machine_root, &paths.kit_root] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.gid() != gid
            || metadata.mode() & 0o777 != 0o750
        {
            return Err(format!(
                "Linux machine service read-only directory has unsafe ownership/mode: {}",
                path.display()
            )
            .into());
        }
    }
    let state = fs::symlink_metadata(&paths.state_root)?;
    if !state.is_dir()
        || state.file_type().is_symlink()
        || state.uid() != uid
        || state.gid() != gid
        || state.mode() & 0o777 != 0o700
    {
        return Err("Linux machine service state directory has unsafe ownership/mode".into());
    }
    for path in [&paths.site_file, &paths.config_file] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.gid() != gid
            || metadata.mode() & 0o777 != 0o640
        {
            return Err(format!(
                "Linux machine service read-only file has unsafe ownership/mode: {}",
                path.display()
            )
            .into());
        }
    }
    verify_readonly_kit_tree(&paths.kit_root, gid)?;
    let binary = fs::symlink_metadata(&paths.binary)?;
    if !binary.is_file()
        || binary.file_type().is_symlink()
        || binary.uid() != 0
        || binary.gid() != 0
        || binary.mode() & 0o777 != 0o755
    {
        return Err("Linux machine service binary has unsafe ownership/mode".into());
    }
    Ok(())
}

fn verify_readonly_kit_tree(root: &Path, gid: u32) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;

    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0_usize;
    while let Some(path) = pending.pop() {
        seen = seen.saturating_add(1);
        if seen > 1024 {
            return Err("Linux machine Site Kit tree exceeds the 1024-entry safety bound".into());
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || metadata.uid() != 0 || metadata.gid() != gid {
            return Err(format!(
                "Linux machine Site Kit ownership/type is unsafe: {}",
                path.display()
            )
            .into());
        }
        if metadata.is_dir() {
            if metadata.mode() & 0o777 != 0o750 {
                return Err(format!(
                    "Linux machine Site Kit directory mode is unsafe: {}",
                    path.display()
                )
                .into());
            }
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            if metadata.mode() & 0o777 != 0o640 {
                return Err(format!(
                    "Linux machine Site Kit file mode is unsafe: {}",
                    path.display()
                )
                .into());
            }
        } else {
            return Err(format!(
                "Linux machine Site Kit contains an unsupported file type: {}",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn create_dir_with_mode(path: &Path, mode: u32) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn copy_binary(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let parent = destination
        .parent()
        .ok_or("installed binary path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".clew.tmp-{}", std::process::id()));
    let result = (|| -> Result<(), Box<dyn Error>> {
        fs::copy(source, &temp)?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(&temp, destination)?;
        sync_parent(destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn copy_site_kit(
    source_site: &Path,
    site: &SignedSiteClew,
    destination_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let source_root = source_site
        .parent()
        .ok_or("Site Kit path has no parent directory")?;
    let installed_site = site.to_bytes()?;
    write_atomic(&destination_root.join("site.clew"), &installed_site, 0o640)?;

    if let Some(profile) = &site.payload.outfit_profile {
        let source_assets = source_root.join("outfit-assets");
        let destination_assets = destination_root.join("outfit-assets");
        for asset_id in profile.imported_asset_ids() {
            let (extension, source) = unique_asset_path(&source_assets, &asset_id)?;
            let mut bytes = Vec::new();
            fs::File::open(&source)?
                .take(16 * 1024 * 1024)
                .read_to_end(&mut bytes)?;
            verify_outfit_asset_bytes(&asset_id, &bytes)?;
            create_dir_with_mode(&destination_assets, 0o750)?;
            let destination = destination_assets.join(format!("{asset_id}.{extension}"));
            write_atomic(&destination, &bytes, 0o640)?;
        }
    }

    for name in [
        NEARBY_CONNECTOR_FILE_NAME,
        LEGACY_NEARBY_CONNECTOR_FILE_NAME,
    ] {
        let source = source_root.join(name);
        if source.is_file() {
            let bytes = read_bounded_file(&source, MAX_NEARBY_FILE_BYTES)?;
            write_atomic(&destination_root.join(name), &bytes, 0o640)?;
        }
    }
    Ok(())
}

fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > max_bytes {
        return Err(format!(
            "bounded machine-service input is unsafe: {}",
            path.display()
        )
        .into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "bounded machine-service input grew while reading: {}",
            path.display()
        )
        .into());
    }
    Ok(bytes)
}

fn unique_asset_path(
    root: &Path,
    asset_id: &str,
) -> Result<(&'static str, PathBuf), Box<dyn Error>> {
    let mut found = None;
    for extension in ["png", "svg"] {
        let candidate = root.join(format!("{asset_id}.{extension}"));
        if candidate.is_file() {
            if found.is_some() {
                return Err(format!("multiple Site Kit assets found for {asset_id}").into());
            }
            found = Some((extension, candidate));
        }
    }
    found.ok_or_else(|| format!("missing Site Kit asset {asset_id}").into())
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("managed file path has no parent")?;
    fs::create_dir_all(parent)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or("managed file name is invalid")?;
    let temp = parent.join(format!(".{name}.tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        fs::rename(&temp, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn sync_parent(path: &Path) -> Result<(), Box<dyn Error>> {
    fs::File::open(path.parent().ok_or("managed file has no parent")?)?.sync_all()?;
    Ok(())
}

fn remove_tree_if_exists(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn command_text(program: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(format!("{program} failed with {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn run_checked(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
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

fn first_existing_program(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

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
    fn system_unit_is_connector_only_low_privilege_and_explicit() {
        let unit = render_system_unit();
        assert!(unit.starts_with(MANAGED_HEADER));
        assert!(unit.contains("User=clew-service"));
        assert!(unit.contains("Group=clew-service"));
        assert!(unit.contains("--foreground --connector-only"));
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("Environment=HOME=/var/lib/clew-service/state"));
        assert!(unit.contains("WorkingDirectory=/var/lib/clew-service/state"));
        assert!(unit.contains("ReadWritePaths=/var/lib/clew-service/state"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("CapabilityBoundingSet=\n"));
        assert!(unit.contains("AmbientCapabilities=\n"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(!unit.contains("--allow-shell"));
        assert!(!unit.contains("--allow-write"));
    }

    #[test]
    fn sysusers_definition_and_metadata_are_strict() {
        let identity = ServiceIdentity { uid: 952, gid: 952 };
        let sysusers = render_sysusers(identity);
        assert!(sysusers.starts_with(MANAGED_HEADER));
        assert!(sysusers.contains("g clew-service 952\n"));
        assert!(
            sysusers.contains(
                "u clew-service 952:952 \"Clew Connector Service\" /var/lib/clew-service"
            )
        );
        assert_eq!(
            parse_managed_sysusers(sysusers.as_bytes()).unwrap(),
            identity
        );
        assert!(parse_managed_sysusers(sysusers.trim_end().as_bytes()).is_err());
        assert!(parse_managed_sysusers(sysusers.replace("952:952", "952:951").as_bytes()).is_err());

        let config = MachineServiceConfig {
            schema_version: SERVICE_SCHEMA_VERSION,
            unit_name: SYSTEM_SERVICE_UNIT.into(),
            service_account: SERVICE_ACCOUNT.into(),
            role: SERVICE_ROLE.into(),
            installed_version: "0.1.0".into(),
            service_uid: 997,
            service_gid: 997,
            binary_sha256: "a".repeat(64),
            site_sha256: "b".repeat(64),
        };
        assert!(config.validate().is_ok());
        let mut wrong = config.clone();
        wrong.service_uid = 0;
        assert!(wrong.validate().is_err());
        let mut mismatched = config;
        mismatched.service_gid = 996;
        assert!(mismatched.validate().is_err());
    }

    #[test]
    fn service_identity_allocator_reserves_orphan_primary_gids() {
        let passwd = concat!(
            "root:x:0:0:root:/root:/bin/bash\n",
            "davfs2:x:956:953:Davfs:/var/cache/davfs2:/sbin/nologin\n",
            "other:x:951:951:Other:/nonexistent:/sbin/nologin\n",
        );
        let group = concat!("root:x:0:\n", "other:x:951:\n", "occupied:x:954:\n",);
        let identity = allocate_service_identity_from(passwd, group, 950, 955).unwrap();
        assert_eq!(identity, ServiceIdentity { uid: 955, gid: 955 });

        let identity = allocate_service_identity_from(passwd, group, 950, 954).unwrap();
        assert_eq!(identity, ServiceIdentity { uid: 952, gid: 952 });
        assert_ne!(
            identity.gid, 953,
            "orphan passwd primary GID must be reserved"
        );
    }

    #[test]
    fn system_id_range_uses_uid_gid_intersection_and_is_bounded() {
        let defs = "SYS_UID_MIN 201\nSYS_UID_MAX 999\nSYS_GID_MIN 300\nSYS_GID_MAX 899\n";
        assert_eq!(system_id_range(defs).unwrap(), (300, 899));
        assert_eq!(system_id_range("").unwrap(), (1, 999));
        assert!(system_id_range("SYS_UID_MIN 1000\nSYS_UID_MAX 900\n").is_err());
    }
}
