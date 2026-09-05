use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use clew_core::ActivityResult;
use clew_host::OutfitProfile;
use clew_runtime::{
    ActivityList, BackupExportRequest, ClientFlavorArtifactList, ClientFlavorArtifactSummary,
    ClientFlavorImportRequest, ControllerConfig, ControllerStatus, DeviceList, InviteIssueRequest,
    LocalApiClient, OutfitAssetImportRequest, OutfitAssetInfo, OutfitAssetList,
    OutfitAssetPreviewResponse, OutfitCloneRequest, OutfitCreateRequest, OutfitList,
    OutfitSetAssetRequest, OutfitUpdateRequest, RecoveryStatus, ReleasePlatform,
    SITE_KIT_LAUNCHER_SCHEMA_VERSION, SiteKitCreateRequest, SiteKitCreateResult,
};

use crate::{
    invite_io,
    studio::{StudioAction, StudioState},
};
use eframe::egui;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const INVITE_MAX_CLAIMS: u32 = 8;
const INVITE_VALID_FOR_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const INVITE_DEPLOYMENT_WINDOW_MS: u64 = 24 * 60 * 60 * 1_000;
const INVITE_MAX_RESULT_BYTES: u32 = 49_152;
const INVITE_READ_TIMEOUT_MS: u32 = 5_000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MCP_HTTP_LISTEN: &str = "127.0.0.1:4877";
const MCP_HTTP_URL: &str = "http://127.0.0.1:4877/mcp";
const CONTROLLER_START_TIMEOUT: Duration = Duration::from_secs(8);
const CONTROLLER_START_POLL: Duration = Duration::from_millis(100);

fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

pub async fn run(config: ControllerConfig) -> Result<(), Box<dyn std::error::Error>> {
    ensure_controller(&config).await?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 700.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Clew",
        options,
        Box::new(move |cc| Ok(Box::new(ControllerApp::new(cc, config.clone())?))),
    )?;
    Ok(())
}

async fn ensure_controller(config: &ControllerConfig) -> Result<(), Box<dyn std::error::Error>> {
    if LocalApiClient::new(config.clone())
        .controller_status()
        .await
        .is_ok()
    {
        return Ok(());
    }

    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("controller")
        .arg("--state-dir")
        .arg(config.state_root());
    configure_background_command(&mut command);
    let mut child = command.spawn()?;
    let client = LocalApiClient::new(config.clone());
    let started = Instant::now();
    loop {
        if client.controller_status().await.is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("Controller exited before becoming ready: {status}").into());
        }
        if started.elapsed() >= CONTROLLER_START_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Controller did not become ready within 8 seconds".into());
        }
        tokio::time::sleep(CONTROLLER_START_POLL).await;
    }
}

enum BackendCommand {
    Refresh,
    InviteIssue {
        request: InviteIssueRequest,
        output: PathBuf,
    },
    OutfitShow(String),
    OutfitCreate(OutfitCreateRequest),
    OutfitClone(OutfitCloneRequest),
    OutfitUpdate(OutfitUpdateRequest),
    OutfitSetDefault(String),
    ClientFlavorImport(PathBuf),
    SiteKitCreate(SiteKitCreateRequest),
    OutfitAssetImport(String),
    OutfitSetAsset(OutfitSetAssetRequest),
    OutfitAssetPreview(String),
    BackupExport {
        path: String,
        passphrase: String,
    },
    RecoveryConfirm,
    ActivityClear,
    Shutdown,
}

enum BackendEvent {
    Snapshot {
        status: ControllerStatus,
        devices: DeviceList,
        activity: ActivityList,
        recovery: RecoveryStatus,
        outfits: OutfitList,
        outfit_assets: OutfitAssetList,
        client_flavors: ClientFlavorArtifactList,
    },
    InviteIssued {
        path: PathBuf,
        site_name: String,
    },
    OutfitProfileLoaded(OutfitProfile),
    OutfitProfileSaved {
        profile: OutfitProfile,
        notice: String,
    },
    OutfitDefaultChanged(String),
    ClientFlavorImported(ClientFlavorArtifactSummary),
    SiteKitCreated(SiteKitCreateResult),
    OutfitAssetImported(OutfitAssetInfo),
    OutfitAssetPreview(OutfitAssetPreviewResponse),
    BackupExportComplete(String),
    RecoveryConfirmed(RecoveryStatus),
    ActivityCleared,
    Error(String),
    ShutdownComplete,
}

struct Backend {
    tx: Sender<BackendCommand>,
    rx: Receiver<BackendEvent>,
}

impl Backend {
    fn start(config: ControllerConfig, ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("clew-gui-local-api".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build GUI Local API runtime");
                let client = LocalApiClient::new(config);
                while let Ok(command) = command_rx.recv() {
                    let shutdown = matches!(&command, BackendCommand::Shutdown);
                    let event = match command {
                        BackendCommand::Refresh => runtime.block_on(async {
                            let result = async {
                                let status = client.controller_status().await?;
                                let devices = client.device_list().await?;
                                let activity = client.activity_list(20).await?;
                                let recovery = client.recovery_status().await?;
                                let outfits = client.outfit_list().await?;
                                let outfit_assets = client.outfit_asset_list().await?;
                                let client_flavors = client.client_flavor_list().await?;
                                Ok::<_, clew_runtime::LocalApiClientError>((
                                    status,
                                    devices,
                                    activity,
                                    recovery,
                                    outfits,
                                    outfit_assets,
                                    client_flavors,
                                ))
                            }
                            .await;
                            match result {
                                Ok((
                                    status,
                                    devices,
                                    activity,
                                    recovery,
                                    outfits,
                                    outfit_assets,
                                    client_flavors,
                                )) => BackendEvent::Snapshot {
                                    status,
                                    devices,
                                    activity,
                                    recovery,
                                    outfits,
                                    outfit_assets,
                                    client_flavors,
                                },
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::InviteIssue { request, output } => {
                            runtime.block_on(async {
                                let site_name = request.site_name.clone();
                                match client.invite_issue(request).await {
                                    Ok(result) => match invite_io::write_invitation(
                                        &client,
                                        &result.site_file,
                                        &output,
                                    )
                                    .await
                                    {
                                        Ok(()) => BackendEvent::InviteIssued {
                                            path: output,
                                            site_name,
                                        },
                                        Err(error) => BackendEvent::Error(error.to_string()),
                                    },
                                    Err(error) => BackendEvent::Error(error.to_string()),
                                }
                            })
                        }
                        BackendCommand::ClientFlavorImport(path) => runtime.block_on(async {
                            let Some(path) = path.to_str() else {
                                return BackendEvent::Error(
                                    "The ClientFlavor cache path must be valid UTF-8.".into(),
                                );
                            };
                            match client
                                .client_flavor_import(ClientFlavorImportRequest {
                                    path: path.to_owned(),
                                })
                                .await
                            {
                                Ok(summary) => BackendEvent::ClientFlavorImported(summary),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::SiteKitCreate(request) => runtime.block_on(async {
                            match client.site_kit_create(request).await {
                                Ok(result) => BackendEvent::SiteKitCreated(result),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitShow(outfit_id) => runtime.block_on(async {
                            match client.outfit_show(outfit_id).await {
                                Ok(profile) => BackendEvent::OutfitProfileLoaded(profile),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitCreate(request) => runtime.block_on(async {
                            match client.outfit_create(request).await {
                                Ok(profile) => BackendEvent::OutfitProfileSaved {
                                    profile,
                                    notice: "Outfit created.".into(),
                                },
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitClone(request) => runtime.block_on(async {
                            match client.outfit_clone(request).await {
                                Ok(profile) => BackendEvent::OutfitProfileSaved {
                                    profile,
                                    notice: "Outfit cloned.".into(),
                                },
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitUpdate(request) => runtime.block_on(async {
                            match client.outfit_update(request).await {
                                Ok(profile) => BackendEvent::OutfitProfileSaved {
                                    profile,
                                    notice: "Outfit changes saved.".into(),
                                },
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitSetDefault(outfit_id) => runtime.block_on(async {
                            match client.outfit_set_default(outfit_id.clone()).await {
                                Ok(()) => BackendEvent::OutfitDefaultChanged(outfit_id),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitAssetImport(path) => runtime.block_on(async {
                            match client
                                .outfit_asset_import(OutfitAssetImportRequest { path })
                                .await
                            {
                                Ok(info) => BackendEvent::OutfitAssetImported(info),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitSetAsset(request) => runtime.block_on(async {
                            match client.outfit_set_asset(request).await {
                                Ok(profile) => BackendEvent::OutfitProfileSaved {
                                    profile,
                                    notice: "Visual asset assigned.".into(),
                                },
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::OutfitAssetPreview(asset_id) => runtime.block_on(async {
                            match client.outfit_asset_preview(asset_id, 192).await {
                                Ok(preview) => BackendEvent::OutfitAssetPreview(preview),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::BackupExport { path, passphrase } => {
                            runtime.block_on(async {
                                match client
                                    .backup_export(BackupExportRequest {
                                        path: path.clone(),
                                        passphrase,
                                    })
                                    .await
                                {
                                    Ok(()) => BackendEvent::BackupExportComplete(path),
                                    Err(error) => BackendEvent::Error(error.to_string()),
                                }
                            })
                        }
                        BackendCommand::RecoveryConfirm => runtime.block_on(async {
                            match client.recovery_confirm().await {
                                Ok(status) => BackendEvent::RecoveryConfirmed(status),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::ActivityClear => runtime.block_on(async {
                            match client.activity_clear().await {
                                Ok(()) => BackendEvent::ActivityCleared,
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::Shutdown => runtime.block_on(async {
                            match client.controller_shutdown().await {
                                Ok(()) => BackendEvent::ShutdownComplete,
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                    };
                    if event_tx.send(event).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                    if shutdown {
                        break;
                    }
                }
            })
            .expect("spawn GUI Local API worker");
        Self {
            tx: command_tx,
            rx: event_rx,
        }
    }

    fn refresh(&self) {
        let _ = self.tx.send(BackendCommand::Refresh);
    }

    fn invite_issue(&self, request: InviteIssueRequest, output: PathBuf) {
        let _ = self
            .tx
            .send(BackendCommand::InviteIssue { request, output });
    }

    fn outfit_show(&self, outfit_id: String) {
        let _ = self.tx.send(BackendCommand::OutfitShow(outfit_id));
    }

    fn outfit_create(&self, request: OutfitCreateRequest) {
        let _ = self.tx.send(BackendCommand::OutfitCreate(request));
    }

    fn outfit_clone(&self, request: OutfitCloneRequest) {
        let _ = self.tx.send(BackendCommand::OutfitClone(request));
    }

    fn outfit_update(&self, request: OutfitUpdateRequest) {
        let _ = self.tx.send(BackendCommand::OutfitUpdate(request));
    }

    fn outfit_set_default(&self, outfit_id: String) {
        let _ = self.tx.send(BackendCommand::OutfitSetDefault(outfit_id));
    }

    fn client_flavor_import(&self, path: PathBuf) {
        let _ = self.tx.send(BackendCommand::ClientFlavorImport(path));
    }

    fn site_kit_create(&self, request: SiteKitCreateRequest) {
        let _ = self.tx.send(BackendCommand::SiteKitCreate(request));
    }

    fn outfit_asset_import(&self, path: String) {
        let _ = self.tx.send(BackendCommand::OutfitAssetImport(path));
    }

    fn outfit_set_asset(&self, request: OutfitSetAssetRequest) {
        let _ = self.tx.send(BackendCommand::OutfitSetAsset(request));
    }

    fn outfit_asset_preview(&self, asset_id: String) {
        let _ = self.tx.send(BackendCommand::OutfitAssetPreview(asset_id));
    }

    fn backup_export(&self, path: String, passphrase: String) {
        let _ = self
            .tx
            .send(BackendCommand::BackupExport { path, passphrase });
    }

    fn recovery_confirm(&self) {
        let _ = self.tx.send(BackendCommand::RecoveryConfirm);
    }

    fn activity_clear(&self) {
        let _ = self.tx.send(BackendCommand::ActivityClear);
    }

    fn shutdown(&self) {
        let _ = self.tx.send(BackendCommand::Shutdown);
    }
}

struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    exit_id: MenuId,
    menu_rx: Receiver<MenuEvent>,
    tray_rx: Receiver<TrayIconEvent>,
}

impl Tray {
    fn new(ctx: &egui::Context) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let menu = Menu::new();
        let show = MenuItem::new("Show Clew", true, None);
        let exit = MenuItem::new("Exit Clew", true, None);
        menu.append(&show)?;
        menu.append(&exit)?;

        let icon = clew_icon()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Clew · Controller ready")
            .with_icon(icon)
            .build()?;

        let (menu_tx, menu_rx) = mpsc::channel();
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = menu_tx.send(event);
            menu_ctx.request_repaint();
        }));
        let (tray_tx, tray_rx) = mpsc::channel();
        let tray_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = tray_tx.send(event);
            tray_ctx.request_repaint();
        }));

        Ok(Self {
            _icon: tray,
            show_id: show.id().clone(),
            exit_id: exit.id().clone(),
            menu_rx,
            tray_rx,
        })
    }
}

fn activity_result_label(result: ActivityResult) -> &'static str {
    match result {
        ActivityResult::Succeeded => "Succeeded",
        ActivityResult::Denied => "Denied",
        ActivityResult::Failed => "Failed",
        ActivityResult::TimedOut => "Timed out",
        ActivityResult::Cancelled => "Cancelled",
    }
}

fn native_release_platform() -> ReleasePlatform {
    #[cfg(windows)]
    {
        ReleasePlatform::Windows
    }
    #[cfg(target_os = "macos")]
    {
        ReleasePlatform::Macos
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        ReleasePlatform::Linux
    }
}

fn release_platform_label(platform: ReleasePlatform) -> &'static str {
    match platform {
        ReleasePlatform::Windows => "Windows",
        ReleasePlatform::Macos => "macOS",
        ReleasePlatform::Linux => "Linux",
    }
}

fn clew_icon() -> Result<Icon, tray_icon::BadIcon> {
    let side = 32_u32;
    let mut rgba = vec![0_u8; (side * side * 4) as usize];
    for y in 0..side {
        for x in 0..side {
            let offset = ((y * side + x) * 4) as usize;
            let inside = (6..26).contains(&x) && (6..26).contains(&y);
            rgba[offset] = if inside { 38 } else { 0 };
            rgba[offset + 1] = if inside { 132 } else { 0 };
            rgba[offset + 2] = if inside { 255 } else { 0 };
            rgba[offset + 3] = if inside { 255 } else { 0 };
        }
    }
    Icon::from_rgba(rgba, side, side)
}

struct ControllerApp {
    backend: Backend,
    controller_config: ControllerConfig,
    tray: Tray,
    status: Option<ControllerStatus>,
    devices: DeviceList,
    activity: ActivityList,
    recovery: RecoveryStatus,
    studio: StudioState,
    outfits: OutfitList,
    client_flavors: ClientFlavorArtifactList,
    invite_open: bool,
    invite_site_name: String,
    invite_read_root: String,
    invite_allow_write: bool,
    invite_allow_shell: bool,
    invite_allow_tcp_egress: bool,
    invite_in_flight: bool,
    client_flavor_import_in_flight: bool,
    mcp_http: Option<Child>,
    mcp_last_error: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    backup_passphrase: String,
    backup_passphrase_confirm: String,
    backup_export_in_flight: bool,
    recovery_confirm_armed: bool,
    last_refresh: Instant,
    refresh_in_flight: bool,
    exit_requested: bool,
}

impl ControllerApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        config: ControllerConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let backend = Backend::start(config.clone(), cc.egui_ctx.clone());
        let tray = Tray::new(&cc.egui_ctx)?;
        backend.refresh();
        Ok(Self {
            backend,
            controller_config: config,
            tray,
            status: None,
            devices: DeviceList {
                devices: Vec::new(),
            },
            activity: ActivityList { events: Vec::new() },
            recovery: RecoveryStatus { review: None },
            studio: StudioState::new(),
            outfits: OutfitList {
                entries: Vec::new(),
                default_outfit_id: String::new(),
                recent_outfit_id: None,
            },
            client_flavors: ClientFlavorArtifactList {
                entries: Vec::new(),
            },
            invite_open: false,
            invite_site_name: "Collaborator".into(),
            invite_read_root: String::new(),
            invite_allow_write: false,
            invite_allow_shell: false,
            invite_allow_tcp_egress: false,
            invite_in_flight: false,
            client_flavor_import_in_flight: false,
            mcp_http: None,
            mcp_last_error: None,
            error: None,
            notice: None,
            backup_passphrase: String::new(),
            backup_passphrase_confirm: String::new(),
            backup_export_in_flight: false,
            recovery_confirm_armed: false,
            last_refresh: Instant::now(),
            refresh_in_flight: true,
            exit_requested: false,
        })
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.backend.rx.try_recv() {
            self.refresh_in_flight = false;
            match event {
                BackendEvent::Snapshot {
                    status,
                    devices,
                    activity,
                    recovery,
                    outfits,
                    outfit_assets,
                    client_flavors,
                } => {
                    self.status = Some(status);
                    self.devices = devices;
                    self.activity = activity;
                    self.recovery = recovery;
                    self.outfits = outfits.clone();
                    self.client_flavors = client_flavors;
                    self.error = None;
                    if let Some(action) = self.studio.set_catalogs(outfits, outfit_assets) {
                        self.dispatch_studio_action(action);
                    }
                }
                BackendEvent::InviteIssued { path, site_name } => {
                    self.invite_in_flight = false;
                    self.invite_open = false;
                    self.notice = Some(format!(
                        "Signed site.clew for {site_name} saved to {}. This advanced sidecar-only export still requires a matching runtime.",
                        path.display()
                    ));
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::ClientFlavorImported(summary) => {
                    self.client_flavor_import_in_flight = false;
                    self.notice = Some(format!(
                        "Verified runtime imported and activated: {} {} ({}/{})",
                        summary.app_display_name,
                        summary.version,
                        release_platform_label(summary.platform),
                        summary.arch
                    ));
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::SiteKitCreated(result) => {
                    self.invite_in_flight = false;
                    self.invite_open = false;
                    self.notice = Some(format!(
                        "Complete Site Kit created: {}",
                        result.archive_path
                    ));
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::OutfitProfileLoaded(profile) => {
                    let actions = self.studio.accept_profile(profile);
                    for action in actions {
                        self.dispatch_studio_action(action);
                    }
                    self.error = None;
                }
                BackendEvent::OutfitProfileSaved { profile, notice } => {
                    let actions = self.studio.accept_profile(profile);
                    for action in actions {
                        self.dispatch_studio_action(action);
                    }
                    self.notice = Some(notice);
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::OutfitDefaultChanged(outfit_id) => {
                    self.studio.accept_default_change();
                    self.notice = Some(format!("Default Outfit set to {outfit_id}."));
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::OutfitAssetImported(info) => {
                    let actions = self.studio.accept_asset_import(info);
                    for action in actions {
                        self.dispatch_studio_action(action);
                    }
                    self.notice = Some("Outfit asset imported.".into());
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::OutfitAssetPreview(preview) => {
                    if let Err(error) = self.studio.accept_preview(ctx, preview) {
                        self.error = Some(error);
                    }
                }
                BackendEvent::BackupExportComplete(path) => {
                    self.backup_export_in_flight = false;
                    self.notice = Some(format!("Encrypted backup exported: {path}"));
                    self.error = None;
                }
                BackendEvent::RecoveryConfirmed(status) => {
                    self.recovery = status;
                    self.recovery_confirm_armed = false;
                    self.notice =
                        Some("Recovery Review confirmed; restored devices may reconnect.".into());
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::ActivityCleared => {
                    self.activity.events.clear();
                    self.notice = Some("Local Activity history cleared.".into());
                    self.error = None;
                }
                BackendEvent::Error(error) => {
                    self.invite_in_flight = false;
                    self.client_flavor_import_in_flight = false;
                    self.backup_export_in_flight = false;
                    self.studio.accept_error();
                    self.error = Some(error);
                }
                BackendEvent::ShutdownComplete => {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        while let Ok(event) = self.tray.menu_rx.try_recv() {
            if event.id == self.tray.show_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id == self.tray.exit_id {
                self.backend.shutdown();
            }
        }
        while self.tray.tray_rx.try_recv().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        if !self.refresh_in_flight && self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.backend.refresh();
            self.refresh_in_flight = true;
            self.last_refresh = Instant::now();
        }
    }

    fn poll_mcp_http(&mut self) {
        let Some(child) = self.mcp_http.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                self.mcp_http = None;
                self.mcp_last_error = Some(format!("MCP server stopped: {status}"));
            }
            Err(error) => {
                self.mcp_http = None;
                self.mcp_last_error = Some(format!("Could not query MCP server: {error}"));
            }
        }
    }

    fn start_mcp_http(&mut self) {
        self.poll_mcp_http();
        if self.mcp_http.is_some() {
            return;
        }
        let result = (|| -> Result<Child, Box<dyn std::error::Error>> {
            let executable = std::env::current_exe()?;
            let mut command = Command::new(executable);
            command
                .arg("mcp")
                .arg("http")
                .arg("--listen")
                .arg(MCP_HTTP_LISTEN)
                .arg("--state-dir")
                .arg(self.controller_config.state_root());
            configure_background_command(&mut command);
            Ok(command.spawn()?)
        })();
        match result {
            Ok(child) => {
                self.mcp_http = Some(child);
                self.mcp_last_error = None;
            }
            Err(error) => self.mcp_last_error = Some(error.to_string()),
        }
    }

    fn stop_mcp_http(&mut self) {
        if let Some(mut child) = self.mcp_http.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.mcp_last_error = None;
    }

    fn import_current_runtime_if_available(&mut self) {
        if self.active_native_client_flavor().is_some()
            || self.client_flavor_import_in_flight
            || self.invite_in_flight
        {
            return;
        }
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let Some(root) = executable.parent() else {
            return;
        };
        if !root.join("release-manifest.json").is_file() {
            return;
        }
        self.backend.client_flavor_import(root.to_path_buf());
        self.client_flavor_import_in_flight = true;
        self.error = None;
    }

    fn render_mcp(&mut self, ui: &mut egui::Ui) {
        self.poll_mcp_http();
        ui.group(|ui| {
            ui.strong("Agent access (MCP)");
            ui.label("Local Streamable HTTP endpoint");
            ui.horizontal(|ui| {
                ui.monospace(MCP_HTTP_URL);
                if ui.button("Copy URL").clicked() {
                    ui.ctx().copy_text(MCP_HTTP_URL.to_owned());
                }
            });
            ui.small("The listener remains loopback-only. Remote clients should reach it through a secure tunnel rather than exposing it directly to the internet.");
            ui.horizontal(|ui| {
                if self.mcp_http.is_some() {
                    ui.label("Status: Running");
                    if ui.button("Stop MCP").clicked() {
                        self.stop_mcp_http();
                    }
                } else {
                    ui.label("Status: Stopped");
                    if ui.button("Start MCP").clicked() {
                        self.start_mcp_http();
                    }
                }
            });
            if let Some(error) = &self.mcp_last_error {
                ui.label(format!("MCP: {error}"));
            }
        });
    }

    fn render_invite(&mut self, ui: &mut egui::Ui) {
        if !self.invite_open {
            return;
        }
        ui.group(|ui| {
            ui.heading("Create a package to send");
            ui.label("Fill in what the other computer should be allowed to do. Clew signs the invitation and builds the complete Site Kit for you.");
            ui.horizontal(|ui| {
                ui.label("Name for this collaborator/site");
                ui.text_edit_singleline(&mut self.invite_site_name);
            });
            ui.label("Folder on the other computer that Clew may access (absolute path)");
            ui.text_edit_singleline(&mut self.invite_read_root);
            ui.small("Example: D:\\research. This path is on the collaborator's computer, not on this Controller.");

            ui.add_space(8.0);
            ui.strong("Capabilities");
            ui.checkbox(&mut self.invite_allow_write, "Allow changing files inside the approved folder");
            ui.checkbox(&mut self.invite_allow_shell, "Allow running commands on that computer");
            ui.checkbox(&mut self.invite_allow_tcp_egress, "Allow TCP forwarding/proxy from that computer");
            if self.invite_allow_shell || self.invite_allow_tcp_egress {
                ui.small("These are powerful permissions. Only enable them when the collaborator expects this access.");
            }

            ui.separator();
            ui.strong("Runtime to include");
            if let Some(default) = self.default_outfit_entry() {
                ui.label(format!("Appearance: {} · revision {}", default.display_name, default.revision));
            }
            if let Some(artifact) = self.active_native_client_flavor() {
                ui.label(format!(
                    "{} {} · {} · {} · {}",
                    artifact.app_display_name,
                    artifact.version,
                    release_platform_label(artifact.platform),
                    artifact.arch,
                    if artifact.release_ready { "signed/release-ready" } else { "verified unsigned 0.x" }
                ));
                if !artifact.release_ready {
                    ui.small("This 0.x Windows/macOS runtime is unsigned. The recipient may see an operating-system trust warning; a future 1.x signed release will remove that warning.");
                }
            } else if self.client_flavor_import_in_flight {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Verifying this Clew installation...");
                });
            } else {
                ui.label("No matching runtime has been verified yet.");
                if ui.button("Use this Clew installation").clicked() {
                    self.import_current_runtime_if_available();
                    if !self.client_flavor_import_in_flight {
                        self.error = Some("This copy is not an extracted release package. Choose an extracted Clew release folder below.".into());
                    }
                }
            }
            egui::CollapsingHeader::new("Choose another runtime package")
                .default_open(false)
                .show(ui, |ui| {
                    ui.small("Select the root folder of an extracted Clew release. Clew verifies release-manifest.json and every payload hash before using it.");
                    if ui
                        .add_enabled(
                            !self.client_flavor_import_in_flight && !self.invite_in_flight,
                            egui::Button::new("Choose extracted Clew release folder..."),
                        )
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder()
                    {
                        self.backend.client_flavor_import(path);
                        self.client_flavor_import_in_flight = true;
                        self.error = None;
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                let ready = !self.invite_site_name.trim().is_empty()
                    && !self.invite_read_root.trim().is_empty()
                    && self.active_native_client_flavor().is_some()
                    && !self.invite_in_flight
                    && !self.client_flavor_import_in_flight;
                if ui
                    .add_enabled(ready, egui::Button::new("Create complete Site Kit..."))
                    .clicked()
                    && let Some(folder) = rfd::FileDialog::new().pick_folder()
                {
                    self.backend.site_kit_create(SiteKitCreateRequest {
                        invite: self.invite_request(),
                        output_dir: folder.to_string_lossy().into_owned(),
                    });
                    self.invite_in_flight = true;
                    self.error = None;
                }
                if ui.button("Cancel").clicked()
                    && !self.invite_in_flight
                    && !self.client_flavor_import_in_flight
                {
                    self.invite_open = false;
                }
                if self.invite_in_flight {
                    ui.spinner();
                    ui.label("Signing invitation and assembling Site Kit...");
                }
            });

            egui::CollapsingHeader::new("Advanced: export signed site.clew only")
                .default_open(false)
                .show(ui, |ui| {
                    ui.small("Use this only for a different target platform or an existing matching runtime. It does not create a complete Site Kit.");
                    let ready = !self.invite_site_name.trim().is_empty()
                        && !self.invite_read_root.trim().is_empty()
                        && !self.invite_in_flight
                        && !self.client_flavor_import_in_flight;
                    if ui
                        .add_enabled(ready, egui::Button::new("Export site.clew..."))
                        .clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        self.backend
                            .invite_issue(self.invite_request(), folder.join("site.clew"));
                        self.invite_in_flight = true;
                        self.error = None;
                    }
                });
        });
    }

    fn invite_request(&self) -> InviteIssueRequest {
        InviteIssueRequest {
            site_name: self.invite_site_name.trim().to_owned(),
            outfit_id: None,
            target_platform: None,
            target_arch: None,
            roots: vec![self.invite_read_root.trim().to_owned()],
            max_claims: INVITE_MAX_CLAIMS,
            valid_for_ms: INVITE_VALID_FOR_MS,
            deployment_window_ms: INVITE_DEPLOYMENT_WINDOW_MS,
            max_result_bytes: INVITE_MAX_RESULT_BYTES,
            read_timeout_ms: INVITE_READ_TIMEOUT_MS,
            allow_write: self.invite_allow_write,
            allow_shell: self.invite_allow_shell,
            allow_tcp_egress: self.invite_allow_tcp_egress,
        }
    }

    fn default_outfit_entry(&self) -> Option<&clew_runtime::OutfitLibraryEntry> {
        self.outfits
            .entries
            .iter()
            .find(|entry| entry.outfit_id == self.outfits.default_outfit_id)
    }

    fn active_native_client_flavor(&self) -> Option<&ClientFlavorArtifactSummary> {
        matching_active_client_flavor(
            &self.outfits,
            &self.client_flavors,
            env!("CARGO_PKG_VERSION"),
            native_release_platform(),
            std::env::consts::ARCH,
        )
    }

    fn dispatch_studio_action(&mut self, action: StudioAction) {
        match action {
            StudioAction::SelectOutfit(outfit_id) => self.backend.outfit_show(outfit_id),
            StudioAction::Create(request) => self.backend.outfit_create(request),
            StudioAction::Clone(request) => self.backend.outfit_clone(request),
            StudioAction::Update(request) => self.backend.outfit_update(request),
            StudioAction::SetDefault(outfit_id) => self.backend.outfit_set_default(outfit_id),
            StudioAction::ImportAsset => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Outfit asset", &["png", "svg"])
                    .pick_file()
                {
                    match path.to_str() {
                        Some(path) => self.backend.outfit_asset_import(path.to_owned()),
                        None => self.error = Some("The asset path must be valid UTF-8.".into()),
                    }
                }
            }
            StudioAction::SetAsset(request) => self.backend.outfit_set_asset(request),
            StudioAction::PreviewAsset(asset_id) => self.backend.outfit_asset_preview(asset_id),
        }
    }
}

fn matching_active_client_flavor<'a>(
    outfits: &OutfitList,
    client_flavors: &'a ClientFlavorArtifactList,
    version: &str,
    platform: ReleasePlatform,
    arch: &str,
) -> Option<&'a ClientFlavorArtifactSummary> {
    let default = outfits
        .entries
        .iter()
        .find(|entry| entry.outfit_id == outfits.default_outfit_id)?;
    client_flavors.entries.iter().find(|artifact| {
        artifact.active
            && (artifact.release_ready || artifact.version.split('.').next() == Some("0"))
            && (platform != ReleasePlatform::Windows
                || artifact.site_kit_launcher_schema == SITE_KIT_LAUNCHER_SCHEMA_VERSION)
            && artifact.outfit_id == default.outfit_id
            && artifact.outfit_revision == default.revision
            && artifact.version == version
            && artifact.platform == platform
            && artifact.arch == arch
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outfit_list() -> OutfitList {
        OutfitList {
            entries: vec![clew_runtime::OutfitLibraryEntry {
                outfit_id: "lab".into(),
                display_name: "Research Lab".into(),
                revision: 3,
                base_preset: clew_host::OutfitPreset::ResearchLab,
                built_in: false,
                is_default: true,
                is_recent: true,
            }],
            default_outfit_id: "lab".into(),
            recent_outfit_id: Some("lab".into()),
        }
    }

    fn artifact() -> ClientFlavorArtifactSummary {
        ClientFlavorArtifactSummary {
            cache_key: format!("client-flavor-v1-{}", "a".repeat(64)),
            client_flavor_id: "flavor-id".into(),
            outfit_id: "lab".into(),
            outfit_revision: 3,
            build_cache_key: format!("outfit-v1-{}", "b".repeat(64)),
            app_display_name: "Research Connect".into(),
            version: "0.1.0".into(),
            target: "x86_64-pc-windows-msvc".into(),
            platform: ReleasePlatform::Windows,
            arch: "x86_64".into(),
            source_commit: "c".repeat(40),
            site_kit_launcher_schema: SITE_KIT_LAUNCHER_SCHEMA_VERSION,
            release_ready: true,
            active: true,
        }
    }

    #[test]
    fn complete_site_kit_readiness_requires_exact_active_native_flavor() {
        let outfits = outfit_list();
        let exact = artifact();
        let list = ClientFlavorArtifactList {
            entries: vec![exact.clone()],
        };
        assert_eq!(
            matching_active_client_flavor(
                &outfits,
                &list,
                "0.1.0",
                ReleasePlatform::Windows,
                "x86_64",
            ),
            Some(&exact)
        );

        let mut unsigned_zero = exact.clone();
        unsigned_zero.release_ready = false;
        let unsigned_list = ClientFlavorArtifactList {
            entries: vec![unsigned_zero.clone()],
        };
        assert_eq!(
            matching_active_client_flavor(
                &outfits,
                &unsigned_list,
                "0.1.0",
                ReleasePlatform::Windows,
                "x86_64",
            ),
            Some(&unsigned_zero)
        );

        let mut unsigned_major = unsigned_zero.clone();
        unsigned_major.version = "1.0.0".into();
        let unsigned_major_list = ClientFlavorArtifactList {
            entries: vec![unsigned_major],
        };
        assert!(
            matching_active_client_flavor(
                &outfits,
                &unsigned_major_list,
                "1.0.0",
                ReleasePlatform::Windows,
                "x86_64",
            )
            .is_none()
        );

        for mismatched in [
            {
                let mut value = exact.clone();
                value.active = false;
                value
            },
            {
                let mut value = exact.clone();
                value.site_kit_launcher_schema = 1;
                value
            },
            {
                let mut value = exact.clone();
                value.outfit_revision = 2;
                value
            },
            {
                let mut value = exact.clone();
                value.version = "0.0.9".into();
                value
            },
            {
                let mut value = exact.clone();
                value.platform = ReleasePlatform::Linux;
                value
            },
            {
                let mut value = exact.clone();
                value.arch = "aarch64".into();
                value
            },
        ] {
            let list = ClientFlavorArtifactList {
                entries: vec![mismatched.clone()],
            };
            assert!(
                matching_active_client_flavor(
                    &outfits,
                    &list,
                    "0.1.0",
                    ReleasePlatform::Windows,
                    "x86_64",
                )
                .is_none()
            );
        }
    }
}

impl Drop for ControllerApp {
    fn drop(&mut self) {
        self.stop_mcp_http();
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    }
}

impl eframe::App for ControllerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events(&ctx);

        if ctx.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        ui.heading("Clew");
        ui.add_space(8.0);
        if let Some(error) = &self.error {
            ui.label(format!("Controller status: unavailable · {error}"));
        } else if let Some(status) = &self.status {
            ui.label(format!(
                "Controller status: {} · PID {}",
                if status.ready { "Ready" } else { "Not ready" },
                status.pid
            ));
        } else {
            ui.label("Controller status: Connecting...");
        }

        if let Some(notice) = &self.notice {
            ui.label(notice);
        }

        if ui.button("Create package for someone else").clicked() {
            self.invite_open = true;
            self.import_current_runtime_if_available();
        }
        self.render_invite(ui);

        ui.separator();
        self.render_mcp(ui);

        if let Some(review) = self.recovery.review
            && review.remote_access_paused
        {
            ui.separator();
            ui.group(|ui| {
                ui.heading("Recovery Review");
                ui.label(
                    "This Controller was restored from an encrypted backup. Existing DeviceKeys remain paused until you confirm recovery.",
                );
                ui.label(format!("ControllerId: {}", review.restored_controller_id));
                if self.recovery_confirm_armed {
                    ui.horizontal(|ui| {
                        if ui.button("Confirm and resume remote access").clicked() {
                            self.backend.recovery_confirm();
                            self.recovery_confirm_armed = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.recovery_confirm_armed = false;
                        }
                    });
                } else if ui.button("Review and continue...").clicked() {
                    self.recovery_confirm_armed = true;
                }
            });
        }

        ui.separator();
        if self.devices.devices.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(56.0);
                ui.heading("No collaborators yet");
                ui.label(
                    "Send the generated Site Kit to a collaborator; they can open it directly.",
                );
                ui.add_space(12.0);
                ui.label("Use Invite collaborator above to create a complete Site Kit without opening a terminal.");
            });
        } else {
            ui.heading("Devices");
            for device in &self.devices.devices {
                ui.group(|ui| {
                    ui.strong(format!("{} / {}", device.site_name, device.display_name));
                    ui.label(format!(
                        "{} · {}",
                        if device.online {
                            "Connected"
                        } else {
                            "Offline"
                        },
                        if device.executable {
                            "Executable"
                        } else {
                            "Connector"
                        }
                    ));
                });
            }
        }

        ui.separator();
        let mut studio_actions = Vec::new();
        egui::CollapsingHeader::new("Outfit Studio")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("outfit-studio-scroll")
                    .max_height(560.0)
                    .show(ui, |ui| {
                        studio_actions = self.studio.ui(ui);
                    });
            });
        for action in studio_actions {
            self.dispatch_studio_action(action);
        }

        ui.separator();
        ui.collapsing("Activity", |ui| {
            ui.horizontal(|ui| {
                ui.label("Latest 20 local activity summaries");
                if ui.button("Clear").clicked() {
                    self.backend.activity_clear();
                }
            });
            if self.activity.events.is_empty() {
                ui.label("No activity yet.");
            } else {
                for event in self.activity.events.iter().rev() {
                    ui.group(|ui| {
                        ui.strong(format!(
                            "{} · {}",
                            event.operation,
                            activity_result_label(event.result)
                        ));
                        ui.label(format!("DeviceId: {}", event.device_id));
                        if let Some(path) = &event.path_summary {
                            ui.label(format!("Path: {path}"));
                        }
                        ui.label(format!(
                            "{} ms · {} bytes",
                            event.duration_ms, event.transferred_bytes
                        ));
                    });
                }
            }
        });

        ui.separator();
        ui.collapsing("Controller backup", |ui| {
            ui.label("Exports are encrypted with Argon2id + XChaCha20-Poly1305. The passphrase is never written to logs or command-line arguments.");
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_passphrase)
                    .password(true)
                    .hint_text("Backup passphrase (at least 12 bytes)"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_passphrase_confirm)
                    .password(true)
                    .hint_text("Confirm backup passphrase"),
            );
            let passphrase_valid = self.backup_passphrase.len() >= 12
                && self.backup_passphrase == self.backup_passphrase_confirm;
            if ui
                .add_enabled(
                    passphrase_valid && !self.backup_export_in_flight,
                    egui::Button::new(if self.backup_export_in_flight {
                        "Exporting..."
                    } else {
                        "Export encrypted backup..."
                    }),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("clew-controller-backup.json")
                    .save_file()
            {
                if let Some(path) = path.to_str() {
                    self.backend
                        .backup_export(path.to_owned(), self.backup_passphrase.clone());
                    self.backup_export_in_flight = true;
                    self.backup_passphrase.clear();
                    self.backup_passphrase_confirm.clear();
                    self.notice = None;
                } else {
                    self.error = Some("The backup path must be valid UTF-8.".into());
                }
            }
            ui.label("Restore requires a stopped Controller and an empty target state. Use `clew backup-restore` for that offline step, then complete Recovery Review here.");
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Exit Clew").clicked() {
                    self.backend.shutdown();
                }
                if ui.button("Hide to tray").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
    }
}
