use std::{
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use clew_runtime::{ControllerConfig, ControllerStatus, DeviceList, LocalApiClient};
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

#[derive(Debug)]
enum BackendCommand {
    Refresh,
    Shutdown,
}

#[derive(Debug)]
enum BackendEvent {
    Snapshot {
        status: ControllerStatus,
        devices: DeviceList,
    },
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
                    let event = match command {
                        BackendCommand::Refresh => runtime.block_on(async {
                            match client.controller_status().await {
                                Ok(status) => match client.device_list().await {
                                    Ok(devices) => BackendEvent::Snapshot { status, devices },
                                    Err(error) => BackendEvent::Error(error.to_string()),
                                },
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
                    if matches!(command, BackendCommand::Shutdown) {
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
    error: Option<String>,
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
            error: None,
            last_refresh: Instant::now(),
            refresh_in_flight: true,
            exit_requested: false,
        })
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.backend.rx.try_recv() {
            self.refresh_in_flight = false;
            match event {
                BackendEvent::Snapshot { status, devices } => {
                    self.status = Some(status);
                    self.devices = devices;
                    self.error = None;
                }
                BackendEvent::Error(error) => self.error = Some(error),
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
