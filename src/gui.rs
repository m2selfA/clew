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
        let show = MenuItem::new("显示 Clew", true, None);
        let exit = MenuItem::new("退出 Clew", true, None);
        menu.append(&show)?;
        menu.append(&exit)?;

        let icon = clew_icon()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Clew · Controller 已就绪")
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
        ActivityResult::Succeeded => "成功",
        ActivityResult::Denied => "已拒绝",
        ActivityResult::Failed => "失败",
        ActivityResult::TimedOut => "超时",
        ActivityResult::Cancelled => "已取消",
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
                    self.notice = Some(format!("加密备份已导出：{path}"));
                    self.error = None;
                }
                BackendEvent::RecoveryConfirmed(status) => {
                    self.recovery = status;
                    self.recovery_confirm_armed = false;
                    self.notice = Some("Recovery Review 已确认；允许已恢复设备重新连接。".into());
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::ActivityCleared => {
                    self.activity.events.clear();
                    self.notice = Some("本机 Activity 历史已清空。".into());
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
            ui.label(format!("Controller 状态：未就绪 · {error}"));
        } else if let Some(status) = &self.status {
            ui.label(format!(
                "Controller 状态：{} · PID {}",
                if status.ready {
                    "已就绪"
                } else {
                    "未就绪"
                },
                status.pid
            ));
        } else {
            ui.label("Controller 状态：正在连接…");
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
                    "此 Controller 从加密备份恢复。旧 DeviceKey 当前被暂停，确认后才允许重新连接。",
                );
                ui.label(format!("ControllerId：{}", review.restored_controller_id));
                if self.recovery_confirm_armed {
                    ui.horizontal(|ui| {
                        if ui.button("确认并恢复远程访问").clicked() {
                            self.backend.recovery_confirm();
                            self.recovery_confirm_armed = false;
                        }
                        if ui.button("取消").clicked() {
                            self.recovery_confirm_armed = false;
                        }
                    });
                } else if ui.button("检查后继续…").clicked() {
                    self.recovery_confirm_armed = true;
                }
            });
        }

        ui.separator();
        if self.devices.devices.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(56.0);
                ui.heading("还没有合作者");
                ui.label("把生成的 Site Kit 发给对方，对方双击即可。");
                ui.add_space(12.0);
                ui.add_enabled(false, egui::Button::new("邀请合作者（V1）"));
            });
        } else {
            ui.heading("设备");
            for device in &self.devices.devices {
                ui.group(|ui| {
                    ui.strong(format!("{} / {}", device.site_name, device.display_name));
                    ui.label(format!(
                        "{} · {}",
                        if device.online { "已连接" } else { "离线" },
                        if device.executable {
                            "可执行"
                        } else {
                            "连接助手"
                        }
                    ));
                });
            }
        }

        ui.separator();
        ui.collapsing("Activity", |ui| {
            ui.horizontal(|ui| {
                ui.label("最近 20 条本机活动摘要");
                if ui.button("清空").clicked() {
                    self.backend.activity_clear();
                }
            });
            if self.activity.events.is_empty() {
                ui.label("暂无活动记录。");
            } else {
                for event in self.activity.events.iter().rev() {
                    ui.group(|ui| {
                        ui.strong(format!(
                            "{} · {}",
                            event.operation,
                            activity_result_label(event.result)
                        ));
                        ui.label(format!("DeviceId：{}", event.device_id));
                        if let Some(path) = &event.path_summary {
                            ui.label(format!("路径：{path}"));
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
        ui.collapsing("Controller 备份", |ui| {
            ui.label("导出内容使用 Argon2id + XChaCha20-Poly1305 加密；口令不会写入日志或命令行参数。");
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_passphrase)
                    .password(true)
                    .hint_text("备份口令（至少 12 bytes）"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.backup_passphrase_confirm)
                    .password(true)
                    .hint_text("再次输入备份口令"),
            );
            let passphrase_valid = self.backup_passphrase.len() >= 12
                && self.backup_passphrase == self.backup_passphrase_confirm;
            if ui
                .add_enabled(
                    passphrase_valid && !self.backup_export_in_flight,
                    egui::Button::new(if self.backup_export_in_flight {
                        "正在导出…"
                    } else {
                        "导出加密备份…"
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
                    self.error = Some("备份路径必须是有效 UTF-8。".into());
                }
            }
            ui.label("恢复备份必须在 Controller 停止且目标 state 为空时执行；当前使用 `clew backup-restore` 完成该离线安全步骤。之后可在此窗口完成 Recovery Review。 ");
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button("退出 Clew").clicked() {
                    self.backend.shutdown();
                }
                if ui.button("隐藏到托盘").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
    }
}
