use std::{
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use clew_core::ActivityResult;
use clew_runtime::{
    ActivityList, BackupExportRequest, ControllerConfig, ControllerStatus, DeviceList,
    LocalApiClient, RecoveryStatus,
};
use eframe::egui;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run(config: ControllerConfig) -> Result<(), Box<dyn std::error::Error>> {
    ensure_controller(&config).await?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 520.0])
            .with_min_inner_size([560.0, 360.0]),
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
        .arg(config.state_root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _child = command.spawn()?;

    LocalApiClient::new(config.clone())
        .controller_status()
        .await?;
    Ok(())
}

enum BackendCommand {
    Refresh,
    BackupExport { path: String, passphrase: String },
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
    },
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
                                Ok::<_, clew_runtime::LocalApiClientError>((
                                    status, devices, activity, recovery,
                                ))
                            }
                            .await;
                            match result {
                                Ok((status, devices, activity, recovery)) => {
                                    BackendEvent::Snapshot {
                                        status,
                                        devices,
                                        activity,
                                        recovery,
                                    }
                                }
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
    tray: Tray,
    status: Option<ControllerStatus>,
    devices: DeviceList,
    activity: ActivityList,
    recovery: RecoveryStatus,
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
        let backend = Backend::start(config, cc.egui_ctx.clone());
        let tray = Tray::new(&cc.egui_ctx)?;
        backend.refresh();
        Ok(Self {
            backend,
            tray,
            status: None,
            devices: DeviceList {
                devices: Vec::new(),
            },
            activity: ActivityList { events: Vec::new() },
            recovery: RecoveryStatus { review: None },
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
                } => {
                    self.status = Some(status);
                    self.devices = devices;
                    self.activity = activity;
                    self.recovery = recovery;
                    self.error = None;
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
                    self.backup_export_in_flight = false;
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
}

impl Drop for ControllerApp {
    fn drop(&mut self) {
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
                ui.add_enabled(false, egui::Button::new("Invite collaborator (V1)"));
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
