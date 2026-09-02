use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clew_core::{ControllerId, SiteId, StateLayout};
use clew_host::{HostLaunchState, OutfitAssetRef, OutfitProfile, OutfitRuntimeView};
use clew_runtime::{OutfitAssetError, OutfitAssetPreview, OutfitAssetStore};
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
    layout: &StateLayout,
    state: HostLaunchState,
    wake_rx: mpsc::UnboundedReceiver<()>,
) -> Result<HostGuiAction, Box<dyn std::error::Error>> {
    let action = Arc::new(Mutex::new(None));
    let action_for_app = Arc::clone(&action);
    let outfit = state.outfit_runtime_view();
    let visuals = HostVisualAssets::load(layout, &state)?;
    let window_title = outfit.resources.window_title.clone();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([620.0, 430.0])
        .with_min_inner_size([500.0, 320.0]);
    if let Some(icon) = visuals.app_icon.as_ref() {
        viewport = viewport.with_icon(Arc::new(egui::IconData {
            rgba: icon.rgba.clone(),
            width: icon.width,
            height: icon.height,
        }));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        &window_title,
        options,
        Box::new(move |cc| {
            Ok(Box::new(HostApp::new(
                cc,
                state,
                outfit,
                visuals,
                wake_rx,
                action_for_app,
            )?))
        }),
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
        show_label: &str,
        exit_label: &str,
        icon: Icon,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let menu = Menu::new();
        let show = MenuItem::new(show_label, true, None);
        let exit = MenuItem::new(exit_label, true, None);
        menu.append(&show)?;
        menu.append(&exit)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(icon)
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

#[derive(Clone, Default)]
struct HostVisualAssets {
    app_icon: Option<OutfitAssetPreview>,
    tray_icon: Option<OutfitAssetPreview>,
    logo: Option<OutfitAssetPreview>,
    key_visual: Option<OutfitAssetPreview>,
}

impl HostVisualAssets {
    fn load(layout: &StateLayout, state: &HostLaunchState) -> Result<Self, OutfitAssetError> {
        let Some(profile) = state_outfit_profile(state) else {
            return Ok(Self::default());
        };
        if profile.imported_asset_ids().is_empty() {
            return Ok(Self::default());
        }
        let store = OutfitAssetStore::load_or_create(layout.clone())?;
        let app_icon = render_asset_ref(&store, &profile.visuals.app_icon, 96)?;
        let tray_ref = profile
            .visuals
            .tray_icon_base
            .as_ref()
            .unwrap_or(&profile.visuals.app_icon);
        let tray_icon = render_asset_ref(&store, tray_ref, 64)?;
        let logo = profile
            .visuals
            .logo
            .as_ref()
            .map(|asset| render_asset_ref(&store, asset, 192))
            .transpose()?
            .flatten();
        let key_visual = profile
            .visuals
            .key_visual
            .as_ref()
            .map(|asset| render_asset_ref(&store, asset, 192))
            .transpose()?
            .flatten();
        Ok(Self {
            app_icon,
            tray_icon,
            logo,
            key_visual,
        })
    }
}

fn state_outfit_profile(state: &HostLaunchState) -> Option<&OutfitProfile> {
    match state {
        HostLaunchState::Active { membership, .. } => membership.marker.outfit_profile.as_ref(),
        HostLaunchState::AwaitingEnrollment { site_file, .. } => {
            site_file.payload.outfit_profile.as_ref()
        }
        HostLaunchState::AmbiguousMembership { .. } | HostLaunchState::MissingInvite { .. } => None,
    }
}

fn render_asset_ref(
    store: &OutfitAssetStore,
    asset: &OutfitAssetRef,
    max_edge: u32,
) -> Result<Option<OutfitAssetPreview>, OutfitAssetError> {
    match asset {
        OutfitAssetRef::BuiltIn { .. } => Ok(None),
        OutfitAssetRef::Imported { asset_id } => store.render_preview(asset_id, max_edge).map(Some),
    }
}

struct HostApp {
    state: HostLaunchState,
    outfit: OutfitRuntimeView,
    wake_rx: mpsc::UnboundedReceiver<()>,
    tray: Tray,
    app_icon: Option<egui::TextureHandle>,
    logo: Option<egui::TextureHandle>,
    key_visual: Option<egui::TextureHandle>,
    action: Arc<Mutex<Option<HostGuiAction>>>,
    exit_requested: bool,
}

impl HostApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        state: HostLaunchState,
        outfit: OutfitRuntimeView,
        visuals: HostVisualAssets,
        wake_rx: mpsc::UnboundedReceiver<()>,
        action: Arc<Mutex<Option<HostGuiAction>>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tooltip = state
            .site_name()
            .map(|name| format!("{} · {name}", outfit.resources.app_name))
            .unwrap_or_else(|| outfit.resources.app_name.clone());
        let tray_icon = visuals
            .tray_icon
            .as_ref()
            .map(preview_to_tray_icon)
            .transpose()?
            .unwrap_or(clew_icon()?);
        let tray = Tray::new(
            &cc.egui_ctx,
            tooltip,
            &outfit.resources.tray_show,
            &outfit.resources.tray_exit,
            tray_icon,
        )?;
        let app_icon = visuals
            .app_icon
            .as_ref()
            .map(|preview| preview_texture(&cc.egui_ctx, "host-app-icon", preview))
            .transpose()?;
        let logo = visuals
            .logo
            .as_ref()
            .map(|preview| preview_texture(&cc.egui_ctx, "host-logo", preview))
            .transpose()?;
        let key_visual = visuals
            .key_visual
            .as_ref()
            .map(|preview| preview_texture(&cc.egui_ctx, "host-key-visual", preview))
            .transpose()?;
        Ok(Self {
            state,
            outfit,
            wake_rx,
            tray,
            app_icon,
            logo,
            key_visual,
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

        let outfit = self.outfit.clone();
        let primary =
            parse_color(&outfit.primary_color).unwrap_or_else(|| ui.visuals().selection.bg_fill);
        if let Some(texture) = &self.app_icon {
            ui.horizontal(|ui| {
                ui.add(egui::Image::new((texture.id(), fit_texture(texture, 48.0))));
                ui.heading(&outfit.resources.app_name);
            });
        } else {
            ui.heading(&outfit.resources.app_name);
        }
        let (accent_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
        ui.painter().rect_filled(accent_rect, 2.0, primary);
        if let Some(texture) = &self.logo {
            ui.add(egui::Image::new((
                texture.id(),
                fit_texture(texture, 120.0),
            )));
        }
        ui.add_space(8.0);
        match &self.state {
            HostLaunchState::Active { membership, .. } => {
                ui.heading(&membership.marker.site_name);
                ui.label(format!(
                    "{} · {}",
                    membership.device.display_name, &outfit.resources.ready
                ));
                ui.add_space(10.0);
                ui.label(format!("DeviceId: {}", membership.marker.device_id));
                ui.label("Local identity restored. Closing this window only hides it to the tray.");
            }
            HostLaunchState::AwaitingEnrollment {
                site_file,
                hostname,
                ..
            } => {
                ui.heading(&site_file.payload.bootstrap.payload.site_name);
                ui.label(&outfit.resources.awaiting_enrollment);
                if let Some(texture) = &self.key_visual {
                    ui.add(egui::Image::new((
                        texture.id(),
                        fit_texture(texture, 160.0),
                    )));
                }
                ui.add_space(10.0);
                ui.label(format!("Device name: {hostname}"));
                ui.label(
                    "The DeviceKey is stored in this operating-system user's Clew state, not in the Site Kit directory.",
                );
            }
            HostLaunchState::MissingInvite { view, .. } => {
                ui.heading(&view.title);
                ui.label(&view.body);
                if let Some(texture) = &self.key_visual {
                    ui.add(egui::Image::new((
                        texture.id(),
                        fit_texture(texture, 160.0),
                    )));
                }
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
                ui.label("You can also drop site.clew directly onto this window.");
            }
            HostLaunchState::AmbiguousMembership { candidates, .. } => {
                ui.heading("Multiple local Clew memberships found");
                ui.label("Choose the Site to open. Clew will not guess the first match.");
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
                if ui.button(&outfit.resources.exit_and_disconnect).clicked() {
                    self.request_action(&ctx, HostGuiAction::Exit);
                }
                if ui.button(&outfit.resources.hide_to_tray).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });
        });
    }
}

fn preview_texture(
    ctx: &egui::Context,
    label: &str,
    preview: &OutfitAssetPreview,
) -> Result<egui::TextureHandle, Box<dyn std::error::Error + Send + Sync>> {
    let width = usize::try_from(preview.width)?;
    let height = usize::try_from(preview.height)?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("outfit preview dimensions overflow")?;
    if width == 0 || height == 0 || preview.rgba.len() != expected {
        return Err("outfit preview dimensions do not match RGBA payload".into());
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &preview.rgba);
    Ok(ctx.load_texture(
        format!("{label}-{}", preview.asset_id),
        image,
        egui::TextureOptions::LINEAR,
    ))
}

fn preview_to_tray_icon(preview: &OutfitAssetPreview) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(preview.rgba.clone(), preview.width, preview.height)
}

fn fit_texture(texture: &egui::TextureHandle, max_edge: f32) -> egui::Vec2 {
    let size = texture.size_vec2();
    let scale = (max_edge / size.x.max(size.y)).min(1.0);
    size * scale
}

fn parse_color(value: &str) -> Option<egui::Color32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(red, green, blue))
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
