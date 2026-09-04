use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use clew_core::StateLayout;
use clew_host::{
    HostLaunchContext, HostLaunchMode, HostLaunchState, LEGACY_NEARBY_CONNECTOR_FILE_NAME,
    NEARBY_CONNECTOR_FILE_NAME, SignedSiteClew, resolve_host_launch_with_mode,
    serve_networked_membership_until_with_layout, verify_outfit_asset_bytes,
    wait_for_networked_activation_until,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceAction as WindowsFailureAction, ServiceActionType, ServiceControl,
        ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
        ServiceFailureResetPeriod, ServiceInfo, ServiceSidType, ServiceStartType, ServiceState,
        ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use super::{ServiceAction, ServiceReport};

mod acl;

pub const SERVICE_PROCESS_ARGUMENT: &str = "__clew-windows-service";
const SERVICE_NAME: &str = "ClewConnector";
const SERVICE_DISPLAY_NAME: &str = "Clew Connector";
const SERVICE_DESCRIPTION: &str =
    "Clew long-lived connector helper. Runs with Connector-only authority under LocalService.";
const SERVICE_ACCOUNT: &str = r"NT AUTHORITY\LocalService";
const SERVICE_SCHEMA_VERSION: u32 = 1;
const SERVICE_ROLE: &str = "connector_only";
const SERVICE_DELETE_TIMEOUT: Duration = Duration::from_secs(10);
const SERVICE_TRANSITION_TIMEOUT: Duration = Duration::from_secs(20);
const SERVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(60 * 60);
const MAX_NEARBY_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
struct MachinePaths {
    root: PathBuf,
    binary: PathBuf,
    kit_root: PathBuf,
    site_file: PathBuf,
    state_root: PathBuf,
    config_file: PathBuf,
}

impl MachinePaths {
    fn current() -> Result<Self, Box<dyn Error>> {
        let program_data = env::var_os("ProgramData")
            .ok_or("required environment variable ProgramData is not set")?;
        let root = PathBuf::from(program_data).join("Clew").join("Service");
        Ok(Self {
            binary: root.join("bin").join("clew.exe"),
            kit_root: root.join("kit"),
            site_file: root.join("kit").join("site.clew"),
            state_root: root.join("state"),
            config_file: root.join("service.json"),
            root,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct MachineServiceConfig {
    schema_version: u32,
    service_name: String,
    role: String,
    installed_version: String,
    binary_sha256: String,
    site_sha256: String,
}

impl MachineServiceConfig {
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.schema_version != SERVICE_SCHEMA_VERSION
            || self.service_name != SERVICE_NAME
            || self.role != SERVICE_ROLE
            || self.installed_version.is_empty()
            || !is_sha256(&self.binary_sha256)
            || !is_sha256(&self.site_sha256)
        {
            return Err("Windows machine service metadata is invalid or incompatible".into());
        }
        Ok(())
    }
}

define_windows_service!(ffi_service_main, service_main);

pub fn run_dispatcher() -> Result<(), Box<dyn Error>> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service_main() {
        eprintln!("Clew Windows service failed: {error}");
    }
}

fn run_service_main() -> Result<(), Box<dyn Error>> {
    let status_slot: Arc<Mutex<Option<ServiceStatusHandle>>> = Arc::new(Mutex::new(None));
    let status_for_handler = Arc::clone(&status_slot);
    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                if let Ok(guard) = status_for_handler.lock()
                    && let Some(handle) = *guard
                {
                    let _ = handle.set_service_status(service_status(ServiceState::StopPending));
                }
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    if let Ok(mut guard) = status_slot.lock() {
        *guard = Some(status_handle);
    }
    status_handle.set_service_status(service_status(ServiceState::StartPending))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    status_handle.set_service_status(service_status(ServiceState::Running))?;
    let result = runtime.block_on(async move {
        let stop_bridge = tokio::spawn(async move {
            let _ = stop_rx.recv().await;
            let _ = shutdown_tx.send(true);
        });
        let result = run_machine_host(shutdown_rx).await;
        stop_bridge.abort();
        let _ = stop_bridge.await;
        result
    });
    let stopped_status = if result.is_ok() {
        service_status(ServiceState::Stopped)
    } else {
        let mut status = service_status(ServiceState::Stopped);
        status.exit_code = ServiceExitCode::Win32(1);
        status
    };
    status_handle.set_service_status(stopped_status)?;
    result
}

fn service_status(state: ServiceState) -> ServiceStatus {
    let controls = if state == ServiceState::Running {
        ServiceControlAccept::STOP
    } else {
        ServiceControlAccept::empty()
    };
    let wait_hint = if matches!(
        state,
        ServiceState::StartPending | ServiceState::StopPending
    ) {
        Duration::from_secs(20)
    } else {
        Duration::default()
    };
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: controls,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

async fn run_machine_host(mut shutdown: watch::Receiver<bool>) -> Result<(), Box<dyn Error>> {
    let paths = MachinePaths::current()?;
    let config = load_machine_config(&paths)?;
    config.validate()?;
    if sha256_file(&paths.binary)? != config.binary_sha256
        || sha256_file(&paths.site_file)? != config.site_sha256
    {
        return Err("Windows machine service files no longer match installed metadata".into());
    }

    let layout = StateLayout::new(paths.state_root.clone());
    let context = HostLaunchContext::current(Some(paths.site_file.clone()), layout.clone())?;
    let state = resolve_host_launch_with_mode(context, HostLaunchMode::ConnectorOnly)?;
    if !state.is_connector_only() {
        return Err("Windows machine service refused a non-Connector-only Host state".into());
    }
    let Some(active) =
        wait_for_networked_activation_until(&layout, state, shutdown.clone()).await?
    else {
        return Ok(());
    };
    if !active.is_connector_only() {
        return Err(
            "Windows machine service enrollment unexpectedly gained EXECUTE authority".into(),
        );
    }
    let HostLaunchState::Active { membership, .. } = active else {
        return Err("Windows machine service could not reach an active Host membership".into());
    };
    if membership.marker.controller_endpoint.is_none() {
        return Err("Windows machine service requires a networked Site Kit".into());
    }
    serve_networked_membership_until_with_layout(&layout, &membership, async move {
        if *shutdown.borrow() {
            return;
        }
        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                return;
            }
        }
    })
    .await?;
    Ok(())
}

pub fn manage_machine(
    action: ServiceAction,
    site: Option<PathBuf>,
) -> Result<ServiceReport, Box<dyn Error>> {
    match action {
        ServiceAction::Status => machine_report(action),
        ServiceAction::Install => {
            let site = site.ok_or("Windows machine service install requires --site FILE")?;
            install_machine(&site)?;
            machine_report(action)
        }
        ServiceAction::Enable => {
            set_start_type(ServiceStartType::AutoStart, true)?;
            machine_report(action)
        }
        ServiceAction::Start => {
            start_machine()?;
            machine_report(action)
        }
        ServiceAction::Stop => {
            stop_machine()?;
            machine_report(action)
        }
        ServiceAction::Disable => {
            set_start_type(ServiceStartType::Disabled, false)?;
            machine_report(action)
        }
        ServiceAction::Uninstall => {
            uninstall_machine()?;
            machine_report(action)
        }
        ServiceAction::EnableLinger | ServiceAction::DisableLinger => {
            Err("linger is unavailable for Windows machine services".into())
        }
    }
}

fn install_machine(site_path: &Path) -> Result<(), Box<dyn Error>> {
    let paths = MachinePaths::current()?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    if service_exists(&manager)? {
        return Err(
            "Clew Windows machine service is already installed; uninstall it before reinstalling"
                .into(),
        );
    }
    if paths.root.exists() {
        return Err(format!(
            "Windows machine service directory already exists: {}; inspect or remove it before install",
            paths.root.display()
        )
        .into());
    }

    let source_site = fs::canonicalize(site_path)?;
    let site_file = SignedSiteClew::read(&source_site)?;
    let _controller = site_file.verify()?;
    if !site_file.payload.bootstrap.payload.grant.member.connector {
        return Err(
            "Site Kit does not grant Connector capability required by the machine service".into(),
        );
    }

    let current_executable = fs::canonicalize(env::current_exe()?)?;
    let service_info = expected_service_info(&paths, ServiceStartType::OnDemand);
    let service_access = ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    let service = manager.create_service(&service_info, service_access)?;
    let install_result = (|| -> Result<(), Box<dyn Error>> {
        service.set_config_service_sid_info(ServiceSidType::Unrestricted)?;
        service.set_description(SERVICE_DESCRIPTION)?;
        configure_failure_actions(&service)?;
        fs::create_dir_all(&paths.root)?;
        harden_machine_root(&paths.root)?;
        fs::create_dir_all(
            paths
                .binary
                .parent()
                .ok_or("service binary path has no parent")?,
        )?;
        fs::create_dir_all(&paths.kit_root)?;
        fs::create_dir_all(&paths.state_root)?;
        fs::copy(&current_executable, &paths.binary)?;
        copy_site_kit(&source_site, &site_file, &paths.kit_root)?;
        let config = MachineServiceConfig {
            schema_version: SERVICE_SCHEMA_VERSION,
            service_name: SERVICE_NAME.into(),
            role: SERVICE_ROLE.into(),
            installed_version: env!("CARGO_PKG_VERSION").into(),
            binary_sha256: sha256_file(&paths.binary)?,
            site_sha256: sha256_file(&paths.site_file)?,
        };
        write_machine_config(&paths, &config)?;
        verify_machine_acl(&paths.root)?;
        Ok(())
    })();
    if let Err(error) = install_result {
        let _ = service.delete();
        drop(service);
        let _ = wait_service_deleted(&manager);
        let _ = fs::remove_dir_all(&paths.root);
        return Err(error);
    }
    Ok(())
}

fn expected_service_info(paths: &MachinePaths, start_type: ServiceStartType) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: paths.binary.clone(),
        launch_arguments: vec![OsString::from(SERVICE_PROCESS_ARGUMENT)],
        dependencies: vec![],
        account_name: Some(OsString::from(SERVICE_ACCOUNT)),
        account_password: None,
    }
}

fn set_start_type(
    start_type: ServiceStartType,
    require_payload: bool,
) -> Result<(), Box<dyn Error>> {
    let paths = if require_payload {
        require_machine_payload()?
    } else {
        require_managed_installation()?
    };
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::CHANGE_CONFIG,
    )?;
    service.change_config(&expected_service_info(&paths, start_type))?;
    Ok(())
}

fn start_machine() -> Result<(), Box<dyn Error>> {
    require_machine_payload()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::START,
    )?;
    let current = service.query_status()?;
    if current.current_state == ServiceState::Running {
        return Ok(());
    }
    if current.current_state != ServiceState::Stopped {
        wait_for_service_state(&service, ServiceState::Stopped, SERVICE_TRANSITION_TIMEOUT)?;
    }
    service.start::<&OsStr>(&[])?;
    wait_for_service_state(&service, ServiceState::Running, SERVICE_TRANSITION_TIMEOUT)
}

fn stop_machine() -> Result<(), Box<dyn Error>> {
    require_managed_installation()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    )?;
    let current = service.query_status()?;
    if current.current_state == ServiceState::Stopped {
        return Ok(());
    }
    if current.current_state != ServiceState::StopPending {
        service.stop()?;
    }
    wait_for_service_state(&service, ServiceState::Stopped, SERVICE_TRANSITION_TIMEOUT)
}

fn uninstall_machine() -> Result<(), Box<dyn Error>> {
    let paths = require_managed_installation()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    let current = service.query_status()?;
    if current.current_state != ServiceState::Stopped {
        if current.current_state != ServiceState::StopPending {
            let _ = service.stop();
        }
        wait_for_service_state(&service, ServiceState::Stopped, SERVICE_TRANSITION_TIMEOUT)?;
    }
    service.delete()?;
    drop(service);
    wait_service_deleted(&manager)?;
    fs::remove_dir_all(&paths.root)?;
    Ok(())
}

fn machine_report(action: ServiceAction) -> Result<ServiceReport, Box<dyn Error>> {
    let paths = MachinePaths::current()?;
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(_) => {
            return Ok(ServiceReport {
                action: action_name(action).into(),
                scope: "machine".into(),
                unit_name: SERVICE_NAME.into(),
                unit_path: paths.binary,
                installed: false,
                managed: false,
                manager_available: false,
                enable_state: None,
                active_state: None,
                linger_enabled: None,
            });
        }
    };
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            return Ok(ServiceReport {
                action: action_name(action).into(),
                scope: "machine".into(),
                unit_name: SERVICE_NAME.into(),
                unit_path: paths.binary,
                installed: false,
                managed: false,
                manager_available: true,
                enable_state: None,
                active_state: None,
                linger_enabled: None,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let status = service.query_status()?;
    let service_config = service.query_config()?;
    let managed = machine_service_identity_matches(&service, &paths)
        .and_then(|matches| {
            if !matches {
                return Ok(false);
            }
            let config = load_machine_config(&paths)?;
            config.validate()?;
            Ok(true)
        })
        .unwrap_or(false);
    Ok(ServiceReport {
        action: action_name(action).into(),
        scope: "machine".into(),
        unit_name: SERVICE_NAME.into(),
        unit_path: paths.binary,
        installed: true,
        managed,
        manager_available: true,
        enable_state: Some(start_type_name(service_config.start_type).into()),
        active_state: Some(service_state_name(status.current_state).into()),
        linger_enabled: None,
    })
}

fn service_exists(manager: &ServiceManager) -> Result<bool, Box<dyn Error>> {
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(_) => Ok(true),
        Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn require_managed_installation() -> Result<MachinePaths, Box<dyn Error>> {
    let paths = MachinePaths::current()?;
    let config = load_machine_config(&paths)?;
    config.validate()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
    )?;
    if !machine_service_identity_matches(&service, &paths)? {
        return Err(
            "refusing to manage a Windows service whose SCM identity does not match Clew".into(),
        );
    }
    Ok(paths)
}

fn require_machine_payload() -> Result<MachinePaths, Box<dyn Error>> {
    let paths = require_managed_installation()?;
    let config = load_machine_config(&paths)?;
    if !paths.binary.is_file() || !paths.site_file.is_file() || !paths.state_root.is_dir() {
        return Err("Windows machine service installation is incomplete".into());
    }
    if sha256_file(&paths.binary)? != config.binary_sha256
        || sha256_file(&paths.site_file)? != config.site_sha256
    {
        return Err(
            "Windows machine service binary or Site Kit no longer matches installed metadata"
                .into(),
        );
    }
    verify_machine_acl(&paths.root)?;
    Ok(paths)
}

fn machine_service_identity_matches(
    service: &windows_service::service::Service,
    paths: &MachinePaths,
) -> Result<bool, Box<dyn Error>> {
    let config = service.query_config()?;
    let command = config.executable_path.to_string_lossy();
    let binary = paths.binary.to_string_lossy();
    let expected_plain = format!("{binary} {SERVICE_PROCESS_ARGUMENT}");
    let expected_quoted = format!("\"{binary}\" {SERVICE_PROCESS_ARGUMENT}");
    let account_matches = config.account_name.as_ref().is_some_and(|account| {
        account
            .to_string_lossy()
            .eq_ignore_ascii_case(SERVICE_ACCOUNT)
    });
    Ok(config.service_type == ServiceType::OWN_PROCESS
        && config.display_name == OsString::from(SERVICE_DISPLAY_NAME)
        && account_matches
        && (command.eq_ignore_ascii_case(&expected_plain)
            || command.eq_ignore_ascii_case(&expected_quoted)))
}

fn configure_failure_actions(
    service: &windows_service::service::Service,
) -> Result<(), Box<dyn Error>> {
    let actions = vec![
        WindowsFailureAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(2),
        },
        WindowsFailureAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(10),
        },
        WindowsFailureAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(60),
        },
        WindowsFailureAction {
            action_type: ServiceActionType::None,
            delay: Duration::default(),
        },
    ];
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_PERIOD),
        reboot_msg: None,
        command: None,
        actions: Some(actions),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    Ok(())
}

fn load_machine_config(paths: &MachinePaths) -> Result<MachineServiceConfig, Box<dyn Error>> {
    let bytes = fs::read(&paths.config_file)?;
    if bytes.len() > 64 * 1024 {
        return Err("Windows machine service metadata is too large".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_machine_config(
    paths: &MachinePaths,
    config: &MachineServiceConfig,
) -> Result<(), Box<dyn Error>> {
    let encoded = serde_json::to_vec_pretty(config)?;
    fs::write(&paths.config_file, encoded)?;
    Ok(())
}

fn copy_site_kit(
    source_site: &Path,
    site_file: &SignedSiteClew,
    destination_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let source_root = source_site
        .parent()
        .ok_or("Site Kit path has no parent directory")?;
    fs::copy(source_site, destination_root.join("site.clew"))?;

    if let Some(profile) = &site_file.payload.outfit_profile {
        let source_assets = source_root.join("outfit-assets");
        let destination_assets = destination_root.join("outfit-assets");
        for asset_id in profile.imported_asset_ids() {
            let (extension, source) = unique_asset_path(&source_assets, &asset_id)?;
            let mut bytes = Vec::new();
            fs::File::open(&source)?
                .take(16 * 1024 * 1024)
                .read_to_end(&mut bytes)?;
            verify_outfit_asset_bytes(&asset_id, &bytes)?;
            fs::create_dir_all(&destination_assets)?;
            fs::write(
                destination_assets.join(format!("{asset_id}.{extension}")),
                bytes,
            )?;
        }
    }

    for name in [
        NEARBY_CONNECTOR_FILE_NAME,
        LEGACY_NEARBY_CONNECTOR_FILE_NAME,
    ] {
        let source = source_root.join(name);
        if source.is_file() {
            let metadata = fs::metadata(&source)?;
            if metadata.len() > MAX_NEARBY_FILE_BYTES {
                return Err(
                    format!("nearby connector file is too large: {}", source.display()).into(),
                );
            }
            fs::copy(&source, destination_root.join(name))?;
        }
    }
    Ok(())
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

fn harden_machine_root(root: &Path) -> Result<(), Box<dyn Error>> {
    let service_sid = service_sid()?;
    acl::apply_protected_directory_dacl(root, &service_sid)?;
    Ok(())
}

fn verify_machine_acl(root: &Path) -> Result<(), Box<dyn Error>> {
    let service_sid = service_sid()?;
    acl::verify_protected_directory_dacl(root, &service_sid)?;
    Ok(())
}

fn service_sid() -> Result<String, Box<dyn Error>> {
    let output = Command::new("sc.exe")
        .args(["showsid", SERVICE_NAME])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to derive Windows service SID: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_service_sid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "sc.exe returned no valid Clew service SID".into())
}

fn parse_service_sid(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| valid_service_sid(token))
        .map(str::to_owned)
}

fn valid_service_sid(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    parts.len() == 9
        && parts[0] == "S"
        && parts[1] == "1"
        && parts[2] == "5"
        && parts[3] == "80"
        && parts[4..].iter().all(|part| part.parse::<u32>().is_ok())
}

fn wait_for_service_state(
    service: &windows_service::service::Service,
    desired: ServiceState,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = service.query_status()?.current_state;
        if current == desired {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Windows service did not reach {} before timeout; current state is {}",
                service_state_name(desired),
                service_state_name(current)
            )
            .into());
        }
        thread::sleep(SERVICE_POLL_INTERVAL);
    }
}

fn wait_service_deleted(manager: &ServiceManager) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + SERVICE_DELETE_TIMEOUT;
    loop {
        match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Err(windows_service::Error::Winapi(error)) if error.raw_os_error() == Some(1060) => {
                return Ok(());
            }
            Ok(service) => drop(service),
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err("Windows service is still pending deletion after timeout".into());
        }
        thread::sleep(SERVICE_POLL_INTERVAL);
    }
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

fn action_name(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Status => "status",
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

fn start_type_name(start_type: ServiceStartType) -> &'static str {
    match start_type {
        ServiceStartType::AutoStart => "auto",
        ServiceStartType::OnDemand => "manual",
        ServiceStartType::Disabled => "disabled",
        ServiceStartType::BootStart => "boot",
        ServiceStartType::SystemStart => "system",
    }
}

fn service_state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "start_pending",
        ServiceState::StopPending => "stop_pending",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "continue_pending",
        ServiceState::PausePending => "pause_pending",
        ServiceState::Paused => "paused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_sid_parser_accepts_only_service_authority_sids() {
        let sample = "NAME: ClewConnector\nSERVICE SID: S-1-5-80-2310617593-914737494-356934921-3458674384-1746943570\nSTATUS: Inactive\n";
        assert_eq!(
            parse_service_sid(sample).as_deref(),
            Some("S-1-5-80-2310617593-914737494-356934921-3458674384-1746943570")
        );
        assert!(parse_service_sid("S-1-5-18").is_none());
        assert!(parse_service_sid("S-1-5-80-1-2-3-4").is_none());
        assert!(parse_service_sid("S-1-5-80-1-2-3-4-nope").is_none());
    }

    #[test]
    fn machine_config_validation_is_strict() {
        let config = MachineServiceConfig {
            schema_version: SERVICE_SCHEMA_VERSION,
            service_name: SERVICE_NAME.into(),
            role: SERVICE_ROLE.into(),
            installed_version: "0.1.0".into(),
            binary_sha256: "a".repeat(64),
            site_sha256: "b".repeat(64),
        };
        assert!(config.validate().is_ok());
        let mut wrong = config.clone();
        wrong.role = "execute".into();
        assert!(wrong.validate().is_err());
        let mut wrong = config;
        wrong.binary_sha256 = "short".into();
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn service_info_is_localservice_connector_process() {
        let paths = MachinePaths {
            root: PathBuf::from(r"C:\ProgramData\Clew\Service"),
            binary: PathBuf::from(r"C:\ProgramData\Clew\Service\bin\clew.exe"),
            kit_root: PathBuf::from(r"C:\ProgramData\Clew\Service\kit"),
            site_file: PathBuf::from(r"C:\ProgramData\Clew\Service\kit\site.clew"),
            state_root: PathBuf::from(r"C:\ProgramData\Clew\Service\state"),
            config_file: PathBuf::from(r"C:\ProgramData\Clew\Service\service.json"),
        };
        let info = expected_service_info(&paths, ServiceStartType::OnDemand);
        assert_eq!(info.name, OsString::from(SERVICE_NAME));
        assert_eq!(info.start_type, ServiceStartType::OnDemand);
        assert_eq!(info.account_name, Some(OsString::from(SERVICE_ACCOUNT)));
        assert_eq!(
            info.launch_arguments,
            vec![OsString::from(SERVICE_PROCESS_ARGUMENT)]
        );
        assert_eq!(info.executable_path, paths.binary);
    }
}
