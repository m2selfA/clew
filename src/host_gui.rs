use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clew_core::{ControllerId, SiteId};
use clew_host::{HostLaunchState, OutfitRuntimeView};
use eframe::egui;
use tokio::sync::mpsc;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

#[derive(Clone, Debug)]
pub enum HostGuiAction {
    Exit,
    OpenSite(PathBuf),
    SelectMembership {
        controller_id: ControllerId,
        site_id: SiteId,
    },
}

pub fn run(
    state: HostLaunchState,
    wake_rx: mpsc::UnboundedReceiver<()>,
) -> Result<HostGuiAction, Box<dyn std::error::Error>> {
    let action = Arc::new(Mutex::new(None));
    let action_for_app = Arc::clone(&action);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 430.0])
            .with_min_inner_size([500.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Clew",
        options,
        Box::new(move |cc| Ok(Box::new(HostApp::new(cc, state, wake_rx, action_for_app)?))),
    )?;
    Ok(action
        .lock()
        .map_err(|_| "host GUI action lock poisoned")?
        .take()
        .unwrap_or(HostGuiAction::Exit))
}

struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    exit_id: MenuId,
    menu_rx: std::sync::mpsc::Receiver<MenuEvent>,
    tray_rx: std::sync::mpsc::Receiver<TrayIconEvent>,
}

impl Tray {
    fn new(
        ctx: &egui::Context,
        tooltip: String,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let menu = Menu::new();
        let show = MenuItem::new("显示 Clew", true, None);
        let exit = MenuItem::new("退出并断开", true, None);
        menu.append(&show)?;
        menu.append(&exit)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(clew_icon()?)
            .build()?;

        let (menu_tx, menu_rx) = std::sync::mpsc::channel();
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = menu_tx.send(event);
            menu_ctx.request_repaint();
        }));
        let (tray_tx, tray_rx) = std::sync::mpsc::channel();
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

struct HostApp {
    state: HostLaunchState,
    wake_rx: mpsc::UnboundedReceiver<()>,
    tray: Tray,
    action: Arc<Mutex<Option<HostGuiAction>>>,
    exit_requested: bool,
}

impl HostApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        state: HostLaunchState,
        wake_rx: mpsc::UnboundedReceiver<()>,
        action: Arc<Mutex<Option<HostGuiAction>>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tooltip = state
            .site_name()
            .map(|name| format!("Clew · {name}"))
            .unwrap_or_else(|| "Clew".into());
        Ok(Self {
            state,
            wake_rx,
            tray: Tray::new(&cc.egui_ctx, tooltip)?,
            action,
            exit_requested: false,
        })
    }

    fn request_action(&mut self, ctx: &egui::Context, action: HostGuiAction) {
        if let Ok(mut slot) = self.action.lock() {
            *slot = Some(action);
        }
        self.exit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn poll_wake_and_tray(&mut self, ctx: &egui::Context) {
        while self.wake_rx.try_recv().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        while let Ok(event) = self.tray.menu_rx.try_recv() {
            if event.id == self.tray.show_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id == self.tray.exit_id {
                self.request_action(ctx, HostGuiAction::Exit);
                return;
            }
        }
        while self.tray.tray_rx.try_recv().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn poll_dropped_site(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        if let Some(path) = dropped
            .into_iter()
            .map(|file| file.path().to_path_buf())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("site.clew"))
            })
        {
            self.request_action(ctx, HostGuiAction::OpenSite(path));
        }
    }
}

impl Drop for HostApp {
    fn drop(&mut self) {
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    }
}

impl eframe::App for HostApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_wake_and_tray(&ctx);
        self.poll_dropped_site(&ctx);
        if ctx.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let outfit = OutfitRuntimeView::clew_original();
        ui.heading(outfit.resources.app_name);
        ui.add_space(8.0);
        match &self.state {
            HostLaunchState::Active { membership, .. } => {
                ui.heading(&membership.marker.site_name);
                ui.label(format!(
                    "{} · {}",
                    membership.device.display_name, outfit.resources.ready
                ));
                ui.add_space(10.0);
                ui.label(format!("DeviceId: {}", membership.marker.device_id));
                ui.label("本机身份已恢复；关闭窗口只会隐藏到托盘。");
            }
            HostLaunchState::AwaitingEnrollment {
                site_file,
                hostname,
                ..
            } => {
                ui.heading(&site_file.payload.bootstrap.payload.site_name);
                ui.label(outfit.resources.awaiting_enrollment);
                ui.add_space(10.0);
                ui.label(format!("设备名：{hostname}"));
                ui.label(
                    "DeviceKey 已保存在当前操作系统用户的 Clew state 中，不在 Site Kit 目录。",
                );
            }
            HostLaunchState::MissingInvite { view, .. } => {
                ui.heading(&view.title);
                ui.label(&view.body);
                if let Some(extract) = &view.extract_first {
                    ui.add_space(8.0);
                    ui.strong(extract);
                }
                ui.add_space(12.0);
                if ui.button(&view.choose_button).clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Clew invitation", &["clew"])
                        .pick_file()
                {
                    self.request_action(&ctx, HostGuiAction::OpenSite(path));
                }
                ui.label("也可以把 site.clew 直接拖到这个窗口。");
            }
            HostLaunchState::AmbiguousMembership { candidates, .. } => {
                ui.heading("找到多个本机 Clew 成员身份");
                ui.label("请选择这次要打开的 Site。Clew 不会猜第一个。");
                ui.add_space(8.0);
                for candidate in candidates {
                    if ui
                        .button(format!("{} · {}", candidate.site_name, candidate.device_id))
                        .clicked()
                    {
                        self.request_action(
                            &ctx,
                            HostGuiAction::SelectMembership {
                                controller_id: candidate.controller.controller_id,
                                site_id: candidate.site_id,
                            },
                        );
                        break;
                    }
                }
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button(outfit.resources.exit_and_disconnect).clicked() {
                    self.request_action(&ctx, HostGuiAction::Exit);
                }
                if ui.button(outfit.resources.hide_to_tray).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
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
