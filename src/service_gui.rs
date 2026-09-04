use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use eframe::egui;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

use crate::service::{self, ServiceAction, ServiceReport, ServiceScope};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 430.0])
            .with_min_inner_size([520.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Clew Background Service",
        options,
        Box::new(|cc| Ok(Box::new(MachineServiceApp::new(cc)?))),
    )?;
    Ok(())
}

enum BackendCommand {
    Refresh,
    Start,
    Stop,
}

enum BackendEvent {
    Snapshot(ServiceReport),
    Error(String),
}

struct Backend {
    tx: Sender<BackendCommand>,
    rx: Receiver<BackendEvent>,
}

impl Backend {
    fn start(ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("clew-machine-service-gui".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build machine service GUI runtime");
                while let Ok(command) = command_rx.recv() {
                    let action = match command {
                        BackendCommand::Refresh => ServiceAction::Status,
                        BackendCommand::Start => ServiceAction::Start,
                        BackendCommand::Stop => ServiceAction::Stop,
                    };
                    let event = match service::manage(action, ServiceScope::Machine, None, None) {
                        Ok(mut report) => {
                            runtime.block_on(service::enrich_report(
                                ServiceScope::Machine,
                                &mut report,
                            ));
                            BackendEvent::Snapshot(report)
                        }
                        Err(error) => BackendEvent::Error(error.to_string()),
                    };
                    let _ = event_tx.send(event);
                    ctx.request_repaint();
                }
            })
            .expect("spawn machine service GUI backend");
        Self {
            tx: command_tx,
            rx: event_rx,
        }
    }

    fn refresh(&self) {
        let _ = self.tx.send(BackendCommand::Refresh);
    }

    fn start_service(&self) {
        let _ = self.tx.send(BackendCommand::Start);
    }

    fn stop_service(&self) {
        let _ = self.tx.send(BackendCommand::Stop);
    }
}

struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    exit_ui_id: MenuId,
    menu_rx: Receiver<MenuEvent>,
    tray_rx: Receiver<TrayIconEvent>,
}

impl Tray {
    fn new(ctx: &egui::Context) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let menu = Menu::new();
        let show = MenuItem::new("Show Clew Background Service", true, None);
        let exit_ui = MenuItem::new("Exit service window", true, None);
        menu.append(&show)?;
        menu.append(&exit_ui)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Clew background service")
            .with_icon(clew_icon()?)
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
            exit_ui_id: exit_ui.id().clone(),
            menu_rx,
            tray_rx,
        })
    }

    fn set_status_tooltip(&self, report: &ServiceReport) {
        let tooltip = if !report.installed {
            "Clew · background service not installed".to_owned()
        } else {
            match report.active_state.as_deref() {
                Some("running") => match report.runtime_state.as_deref() {
                    Some("serving_connector") => report
                        .runtime_site_name
                        .as_deref()
                        .map(|site| format!("Clew · helping {site} connect"))
                        .unwrap_or_else(|| "Clew · background service running".into()),
                    Some("awaiting_enrollment") => "Clew · background service connecting".into(),
                    Some("stopping") => "Clew · background service stopping".into(),
                    _ => "Clew · background service starting".into(),
                },
                Some("stopped") => "Clew · background service stopped".into(),
                _ => "Clew · background service status changing".into(),
            }
        };
        let _ = self._icon.set_tooltip(Some(tooltip));
    }
}

struct MachineServiceApp {
    backend: Backend,
    tray: Tray,
    report: Option<ServiceReport>,
    error: Option<String>,
    notice: Option<String>,
    refresh_in_flight: bool,
    last_refresh: Instant,
    exit_requested: bool,
}

impl MachineServiceApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let backend = Backend::start(cc.egui_ctx.clone());
        let tray = Tray::new(&cc.egui_ctx)?;
        backend.refresh();
        Ok(Self {
            backend,
            tray,
            report: None,
            error: None,
            notice: None,
            refresh_in_flight: true,
            last_refresh: Instant::now(),
            exit_requested: false,
        })
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.backend.rx.try_recv() {
            self.refresh_in_flight = false;
            self.last_refresh = Instant::now();
            match event {
                BackendEvent::Snapshot(report) => {
                    self.tray.set_status_tooltip(&report);
                    self.report = Some(report);
                    self.error = None;
                    self.notice = None;
                }
                BackendEvent::Error(error) => {
                    self.error = Some(error);
                    self.notice = None;
                }
            }
        }

        while let Ok(event) = self.tray.menu_rx.try_recv() {
            if event.id == self.tray.show_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id == self.tray.exit_ui_id {
                self.exit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        while self.tray.tray_rx.try_recv().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        if !self.refresh_in_flight && self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.backend.refresh();
            self.refresh_in_flight = true;
        }
    }

    fn request_start(&mut self) {
        self.notice = Some("Starting the background service...".into());
        self.error = None;
        self.backend.start_service();
        self.refresh_in_flight = true;
    }

    fn request_stop(&mut self) {
        self.notice = Some("Stopping the background service...".into());
        self.error = None;
        self.backend.stop_service();
        self.refresh_in_flight = true;
    }

    fn render_status(&mut self, ui: &mut egui::Ui) {
        let Some(report) = &self.report else {
            ui.label("Checking background service status...");
            return;
        };

        if !report.installed {
            ui.heading("Background service is not installed");
            ui.label("Installation is an explicit administrator action and is never performed by this window.");
            ui.code("clew service install --scope machine --site <site.clew>");
            return;
        }

        let active = report.active_state.as_deref().unwrap_or("unknown");
        let enabled = report.enable_state.as_deref().unwrap_or("unknown");
        ui.heading(match active {
            "running" => "Background service is running",
            "stopped" => "Background service is stopped",
            _ => "Background service is changing state",
        });
        ui.label(format!("Startup: {enabled} · Runtime: {active}"));
        if let Some(pid) = report.process_id {
            ui.label(format!("Service PID: {pid}"));
        }

        if active == "running" {
            match report.control_ipc_available {
                Some(true) => {
                    if let Some(state) = &report.runtime_state {
                        ui.label(format!("Connector state: {}", human_runtime_state(state)));
                    }
                    if let Some(site) = &report.runtime_site_name {
                        ui.label(format!("Site: {site}"));
                    }
                    if let Some(device_id) = &report.runtime_device_id {
                        ui.label(format!("DeviceId: {device_id}"));
                    }
                }
                Some(false) => {
                    ui.label("The service is running, but runtime status is not available yet.");
                }
                None => {}
            }
        }

        ui.add_space(10.0);
        ui.strong("This machine service is Connector-only.");
        ui.label("It does not expose this computer's files or commands to the Controller.");
        ui.label("Closing or exiting this window does not stop the background service.");

        ui.add_space(12.0);
        if active == "running" {
            if ui.button("Stop background service").clicked() {
                self.request_stop();
            }
        } else if active == "stopped" && ui.button("Start background service").clicked() {
            self.request_start();
        }
    }
}

impl Drop for MachineServiceApp {
    fn drop(&mut self) {
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    }
}

impl eframe::App for MachineServiceApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events(&ctx);

        if ctx.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        ui.heading("Clew · Background Service");
        ui.label("Advanced · whole-machine long-lived connection helper");
        ui.separator();
        if let Some(error) = &self.error {
            ui.label(format!("Service status unavailable: {error}"));
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        self.render_status(ui);

        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Exit service window").clicked() {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Hide to tray").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
    }
}

fn human_runtime_state(state: &str) -> &str {
    match state {
        "starting" => "Starting",
        "awaiting_enrollment" => "Connecting",
        "serving_connector" => "Helping nearby computers connect",
        "stopping" => "Stopping",
        _ => "Unknown",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_state_labels_are_human_facing() {
        assert_eq!(
            human_runtime_state("serving_connector"),
            "Helping nearby computers connect"
        );
        assert_eq!(human_runtime_state("awaiting_enrollment"), "Connecting");
    }
}
