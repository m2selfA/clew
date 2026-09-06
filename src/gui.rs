use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use clew_core::{ActivityResult, InviteId, site_access_credential_id};
use clew_host::OutfitProfile;
use clew_runtime::{
    ActivityList, BackupExportRequest, ClientFlavorArtifactList, ClientFlavorArtifactSummary,
    ClientFlavorImportRequest, ControllerConfig, ControllerStatus, DeviceList, InviteIssueRequest,
    LocalApiClient, OutfitAssetImportRequest, OutfitAssetInfo, OutfitAssetList,
    OutfitAssetPreviewResponse, OutfitCloneRequest, OutfitCreateRequest, OutfitList,
    OutfitSetAssetRequest, OutfitUpdateRequest, RecoveryStatus, ReleasePlatform,
    SITE_KIT_LAUNCHER_SCHEMA_VERSION, SiteAccessCredentialList, SiteKitCreateRequest,
    SiteKitCreateResult,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InviteScenario {
    DirectTarget,
    PrivateTargetViaHelper,
}

fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
}

fn configure_light_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = egui::Color32::from_rgb(245, 247, 250);
    visuals.window_fill = egui::Color32::WHITE;
    visuals.faint_bg_color = egui::Color32::from_rgb(241, 245, 249);
    visuals.extreme_bg_color = egui::Color32::WHITE;
    visuals.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
    visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
    visuals.widgets.noninteractive.bg_fill = egui::Color32::WHITE;
    visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(226, 232, 240));
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(248, 250, 252);
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(203, 213, 225));
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(239, 246, 255);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(219, 234, 254);
    ctx.set_visuals_of(egui::Theme::Light, visuals);

    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 34.0;
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    ctx.set_style_of(egui::Theme::Light, style);
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(text)
            .color(egui::Color32::WHITE)
            .strong(),
    )
    .fill(egui::Color32::from_rgb(37, 99, 235))
    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(29, 78, 216)))
}

fn section_card() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::WHITE)
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(226, 232, 240),
        ))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(16))
}

fn option_card(selected: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(if selected {
            egui::Color32::from_rgb(239, 246, 255)
        } else {
            egui::Color32::from_rgb(248, 250, 252)
        })
        .stroke(egui::Stroke::new(
            if selected { 1.5 } else { 1.0 },
            if selected {
                egui::Color32::from_rgb(96, 165, 250)
            } else {
                egui::Color32::from_rgb(226, 232, 240)
            },
        ))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
}

#[derive(Clone, Copy)]
enum FlowGlyph {
    Package,
    Computer,
    Link,
    Agent,
}

#[derive(Clone, Copy)]
enum FlowState {
    Complete,
    Active,
    Pending,
}

fn flow_icon(ui: &mut egui::Ui, glyph: FlowGlyph, state: FlowState) {
    let (fill, foreground) = match state {
        FlowState::Complete => (
            egui::Color32::from_rgb(220, 252, 231),
            egui::Color32::from_rgb(22, 101, 52),
        ),
        FlowState::Active => (
            egui::Color32::from_rgb(219, 234, 254),
            egui::Color32::from_rgb(29, 78, 216),
        ),
        FlowState::Pending => (
            egui::Color32::from_rgb(241, 245, 249),
            egui::Color32::from_rgb(100, 116, 139),
        ),
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
    let center = rect.center();
    let painter = ui.painter();
    painter.circle_filled(center, 18.0, fill);
    let stroke = egui::Stroke::new(2.0, foreground);
    match glyph {
        FlowGlyph::Package => {
            let x = center.x;
            let y = center.y;
            for (a, b) in [
                (egui::pos2(x - 9.0, y - 6.0), egui::pos2(x, y - 11.0)),
                (egui::pos2(x, y - 11.0), egui::pos2(x + 9.0, y - 6.0)),
                (egui::pos2(x - 9.0, y - 6.0), egui::pos2(x - 9.0, y + 7.0)),
                (egui::pos2(x + 9.0, y - 6.0), egui::pos2(x + 9.0, y + 7.0)),
                (egui::pos2(x - 9.0, y + 7.0), egui::pos2(x, y + 12.0)),
                (egui::pos2(x + 9.0, y + 7.0), egui::pos2(x, y + 12.0)),
                (egui::pos2(x - 9.0, y - 6.0), egui::pos2(x, y - 1.0)),
                (egui::pos2(x + 9.0, y - 6.0), egui::pos2(x, y - 1.0)),
                (egui::pos2(x, y - 1.0), egui::pos2(x, y + 12.0)),
            ] {
                painter.line_segment([a, b], stroke);
            }
        }
        FlowGlyph::Computer => {
            let x = center.x;
            let y = center.y;
            for (a, b) in [
                (egui::pos2(x - 11.0, y - 9.0), egui::pos2(x + 11.0, y - 9.0)),
                (egui::pos2(x + 11.0, y - 9.0), egui::pos2(x + 11.0, y + 5.0)),
                (egui::pos2(x + 11.0, y + 5.0), egui::pos2(x - 11.0, y + 5.0)),
                (egui::pos2(x - 11.0, y + 5.0), egui::pos2(x - 11.0, y - 9.0)),
                (egui::pos2(x, y + 5.0), egui::pos2(x, y + 10.0)),
                (egui::pos2(x - 6.0, y + 10.0), egui::pos2(x + 6.0, y + 10.0)),
            ] {
                painter.line_segment([a, b], stroke);
            }
        }
        FlowGlyph::Link => {
            painter.circle_stroke(egui::pos2(center.x - 6.0, center.y), 6.0, stroke);
            painter.circle_stroke(egui::pos2(center.x + 6.0, center.y), 6.0, stroke);
            painter.line_segment(
                [
                    egui::pos2(center.x - 2.0, center.y),
                    egui::pos2(center.x + 2.0, center.y),
                ],
                stroke,
            );
        }
        FlowGlyph::Agent => {
            painter.circle_stroke(center, 7.0, stroke);
            painter.line_segment(
                [
                    egui::pos2(center.x - 11.0, center.y),
                    egui::pos2(center.x - 7.0, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x + 7.0, center.y),
                    egui::pos2(center.x + 11.0, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(center.x, center.y - 11.0),
                    egui::pos2(center.x, center.y - 7.0),
                ],
                stroke,
            );
        }
    }
}

fn workflow_step(
    ui: &mut egui::Ui,
    glyph: FlowGlyph,
    state: FlowState,
    title: &str,
    subtitle: &str,
) {
    ui.horizontal(|ui| {
        flow_icon(ui, glyph, state);
        ui.vertical(|ui| {
            ui.strong(title);
            ui.add(egui::Label::new(egui::RichText::new(subtitle).size(12.5)).wrap());
        });
    });
}

fn status_badge(ui: &mut egui::Ui, text: &str, good: bool) {
    let (fill, foreground) = if good {
        (
            egui::Color32::from_rgb(236, 253, 245),
            egui::Color32::from_rgb(5, 122, 85),
        )
    } else {
        (
            egui::Color32::from_rgb(241, 245, 249),
            egui::Color32::from_rgb(71, 85, 105),
        )
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(99))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").color(foreground));
                ui.label(egui::RichText::new(text).color(foreground).strong());
            });
        });
}

fn muted(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    ui.add(egui::Label::new(text.into()).wrap());
}

pub async fn run(config: ControllerConfig) -> Result<(), Box<dyn std::error::Error>> {
    ensure_controller(&config).await?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 840.0])
            .with_min_inner_size([840.0, 600.0]),
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
    if config.local_acceptance_runtime() {
        command.arg("--local-acceptance-runtime");
    }
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
    CredentialClose(InviteId),
    CredentialRevoke(InviteId),
    CredentialDelete(InviteId),
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
        credentials: SiteAccessCredentialList,
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
    ClientFlavorImportFailed(String),
    SiteKitCreated(SiteKitCreateResult),
    CredentialChanged(String),
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
                                let credentials = client.credential_list().await?;
                                let activity = client.activity_list(20).await?;
                                let recovery = client.recovery_status().await?;
                                let outfits = client.outfit_list().await?;
                                let outfit_assets = client.outfit_asset_list().await?;
                                let client_flavors = client.client_flavor_list().await?;
                                Ok::<_, clew_runtime::LocalApiClientError>((
                                    status,
                                    devices,
                                    credentials,
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
                                    credentials,
                                    activity,
                                    recovery,
                                    outfits,
                                    outfit_assets,
                                    client_flavors,
                                )) => BackendEvent::Snapshot {
                                    status,
                                    devices,
                                    credentials,
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
                                Err(error) => {
                                    BackendEvent::ClientFlavorImportFailed(error.to_string())
                                }
                            }
                        }),
                        BackendCommand::SiteKitCreate(request) => runtime.block_on(async {
                            match client.site_kit_create(request).await {
                                Ok(result) => BackendEvent::SiteKitCreated(result),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::CredentialClose(invite_id) => runtime.block_on(async {
                            match client.invite_close(invite_id).await {
                                Ok(()) => BackendEvent::CredentialChanged(
                                    "Site Access Credential closed to new devices.".into(),
                                ),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::CredentialRevoke(invite_id) => runtime.block_on(async {
                            match client.invite_revoke(invite_id).await {
                                Ok(()) => BackendEvent::CredentialChanged(
                                    "Site Access Credential revoked; its enrolled devices were disconnected.".into(),
                                ),
                                Err(error) => BackendEvent::Error(error.to_string()),
                            }
                        }),
                        BackendCommand::CredentialDelete(invite_id) => runtime.block_on(async {
                            match client.invite_delete(invite_id).await {
                                Ok(()) => BackendEvent::CredentialChanged(
                                    "Old Site Access Credential deleted; a revocation tombstone is retained so the old Site Kit cannot be reused.".into(),
                                ),
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

    fn credential_close(&self, invite_id: InviteId) {
        let _ = self.tx.send(BackendCommand::CredentialClose(invite_id));
    }

    fn credential_revoke(&self, invite_id: InviteId) {
        let _ = self.tx.send(BackendCommand::CredentialRevoke(invite_id));
    }

    fn credential_delete(&self, invite_id: InviteId) {
        let _ = self.tx.send(BackendCommand::CredentialDelete(invite_id));
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialConfirmAction {
    Revoke(InviteId),
    Delete(InviteId),
}

struct ControllerApp {
    backend: Backend,
    controller_config: ControllerConfig,
    tray: Tray,
    status: Option<ControllerStatus>,
    devices: DeviceList,
    credentials: SiteAccessCredentialList,
    selected_credential: Option<InviteId>,
    credential_confirm: Option<CredentialConfirmAction>,
    activity: ActivityList,
    recovery: RecoveryStatus,
    studio: StudioState,
    outfits: OutfitList,
    client_flavors: ClientFlavorArtifactList,
    invite_open: bool,
    invite_scenario: InviteScenario,
    invite_site_name: String,
    invite_read_root: String,
    invite_all_filesystem: bool,
    invite_allow_write: bool,
    invite_allow_shell: bool,
    invite_allow_tcp_egress: bool,
    invite_in_flight: bool,
    client_flavor_import_in_flight: bool,
    current_runtime_import_attempted: bool,
    client_flavor_import_error: Option<String>,
    mcp_http: Option<Child>,
    mcp_last_error: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    last_site_kit_path: Option<String>,
    last_site_kit_scenario: Option<InviteScenario>,
    last_site_kit_invite_id: Option<InviteId>,
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
        configure_light_theme(&cc.egui_ctx);

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
            credentials: SiteAccessCredentialList {
                credentials: Vec::new(),
            },
            selected_credential: None,
            credential_confirm: None,
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
            invite_scenario: InviteScenario::DirectTarget,
            invite_site_name: "Collaborator".into(),
            invite_read_root: String::new(),
            invite_all_filesystem: true,
            invite_allow_write: false,
            invite_allow_shell: false,
            invite_allow_tcp_egress: false,
            invite_in_flight: false,
            client_flavor_import_in_flight: false,
            current_runtime_import_attempted: false,
            client_flavor_import_error: None,
            mcp_http: None,
            mcp_last_error: None,
            error: None,
            notice: None,
            last_site_kit_path: None,
            last_site_kit_scenario: None,
            last_site_kit_invite_id: None,
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
                    credentials,
                    activity,
                    recovery,
                    outfits,
                    outfit_assets,
                    client_flavors,
                } => {
                    self.status = Some(status);
                    self.devices = devices;
                    let selected_present = self.selected_credential.is_some_and(|selected| {
                        credentials
                            .credentials
                            .iter()
                            .any(|credential| credential.invite_id == selected)
                    });
                    if !selected_present {
                        self.selected_credential = credentials
                            .credentials
                            .first()
                            .map(|credential| credential.invite_id);
                        self.credential_confirm = None;
                    }
                    self.credentials = credentials;
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
                    self.client_flavor_import_error = None;
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
                BackendEvent::ClientFlavorImportFailed(error) => {
                    self.client_flavor_import_in_flight = false;
                    self.client_flavor_import_error = Some(error);
                }
                BackendEvent::SiteKitCreated(result) => {
                    self.invite_in_flight = false;
                    self.invite_open = false;
                    self.last_site_kit_path = Some(result.archive_path.clone());
                    self.last_site_kit_scenario = Some(self.invite_scenario);
                    self.last_site_kit_invite_id = Some(result.invite_id);
                    self.selected_credential = Some(result.invite_id);
                    self.credential_confirm = None;
                    self.notice = Some(format!("Site Kit created: {}", result.archive_path));
                    self.error = None;
                    self.backend.refresh();
                    self.refresh_in_flight = true;
                    self.last_refresh = Instant::now();
                }
                BackendEvent::CredentialChanged(notice) => {
                    self.credential_confirm = None;
                    self.notice = Some(notice);
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
        if !should_import_current_runtime(
            self.controller_config.local_acceptance_runtime(),
            self.current_runtime_import_attempted,
            self.active_native_client_flavor().is_some(),
            self.client_flavor_import_in_flight,
            self.invite_in_flight,
        ) {
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
        self.current_runtime_import_attempted = true;
        self.backend.client_flavor_import(root.to_path_buf());
        self.client_flavor_import_in_flight = true;
        self.client_flavor_import_error = None;
        self.error = None;
    }

    fn render_mcp(&mut self, ui: &mut egui::Ui) {
        self.poll_mcp_http();
        ui.label("Connect your local coding/research agent to Clew after target B is online.");
        muted(
            ui,
            "Available through MCP: bounded read/search/edit, durable file and directory transfer, safe Trash/Recycle-Bin operations, managed temporary resources, and Shell when explicitly granted.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.monospace(MCP_HTTP_URL);
            if ui.button("Copy URL").clicked() {
                ui.ctx().copy_text(MCP_HTTP_URL.to_owned());
            }
        });
        muted(
            ui,
            "This listener stays loopback-only on Controller A. Use a secure tunnel for a remote MCP client instead of exposing the port to the Internet.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if self.mcp_http.is_some() {
                status_badge(ui, "MCP running", true);
                if ui.button("Stop MCP").clicked() {
                    self.stop_mcp_http();
                }
            } else {
                status_badge(ui, "MCP stopped", false);
                if ui.add(primary_button("Start MCP")).clicked() {
                    self.start_mcp_http();
                }
            }
        });
        if let Some(error) = &self.mcp_last_error {
            ui.label(
                egui::RichText::new(format!("MCP: {error}"))
                    .color(egui::Color32::from_rgb(185, 28, 28)),
            );
        }
    }

    fn render_invite(&mut self, ui: &mut egui::Ui) {
        if !self.invite_open {
            return;
        }
        ui.vertical(|ui| {
            ui.label("Choose the setup that matches the collaborator's network. These two cases cover the normal Clew workflow.");
            ui.add_space(6.0);
            option_card(self.invite_scenario == InviteScenario::DirectTarget).show(ui, |ui| {
                ui.radio_value(
                    &mut self.invite_scenario,
                    InviteScenario::DirectTarget,
                    "A/B — B can access the Internet",
                );
                muted(ui, "A = this Controller · B = the collaborator's target. Send one Site Kit to B; on B choose “Use this computer”.");
            });
            option_card(self.invite_scenario == InviteScenario::PrivateTargetViaHelper).show(ui, |ui| {
                ui.radio_value(
                    &mut self.invite_scenario,
                    InviteScenario::PrivateTargetViaHelper,
                    "A/B/C — B is private and uses helper C",
                );
                muted(ui, "A = this Controller · B = the private target · C = the collaborator's online helper that can also reach B. Send the same Site Kit to B and C.");
            });
            ui.add_space(8.0);
            match self.invite_scenario {
                InviteScenario::DirectTarget => {
                    ui.strong("You will send one Site Kit to target computer B.");
                }
                InviteScenario::PrivateTargetViaHelper => {
                    ui.strong("You will create one Site Kit and send the same archive to both B and C.");
                    ui.small("Do not create a second package for helper C. Helper mode can only reduce authority; C never receives B's file or shell permissions.");
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Site / collaborator name");
                ui.text_edit_singleline(&mut self.invite_site_name);
            });
            ui.strong("Filesystem access on target B");
            ui.radio_value(
                &mut self.invite_all_filesystem,
                true,
                "All folders visible to B's operating-system account (default)",
            );
            ui.small("Clew does not bypass Windows/macOS/Linux permissions. This only removes Clew's extra root restriction.");
            ui.radio_value(
                &mut self.invite_all_filesystem,
                false,
                "Only an approved folder",
            );
            if !self.invite_all_filesystem {
                ui.label("Approved folder on target B (absolute path)");
                ui.text_edit_singleline(&mut self.invite_read_root);
                ui.small(match self.invite_scenario {
                    InviteScenario::DirectTarget => "Example: D:\\research. This path is on target B, not on Controller A.",
                    InviteScenario::PrivateTargetViaHelper => "Example: D:\\research. This path is on target B, not on Controller A or helper C.",
                });
            }

            ui.add_space(8.0);
            ui.strong("Permissions for target B");
            ui.checkbox(
                &mut self.invite_allow_write,
                "Allow file changes in the selected filesystem scope",
            );
            if self.invite_allow_write {
                muted(ui, "Includes Write/Edit, file upload, mkdir/copy/move, safe Trash, and Clew-managed temporary resources. Permanent deletion still requires a separate two-step confirmation.");
            }
            ui.checkbox(&mut self.invite_allow_shell, "Allow running commands on that computer");
            ui.checkbox(&mut self.invite_allow_tcp_egress, "Allow TCP forwarding/proxy from that computer");
            if self.invite_allow_shell || self.invite_allow_tcp_egress {
                ui.small("These are powerful permissions. Only enable them when the collaborator expects this access.");
            }
            if self.invite_scenario == InviteScenario::PrivateTargetViaHelper {
                ui.small("These permissions apply to target B only. Helper C is always connector-only even when target permissions are enabled.");
            }

            ui.small("Creating a Site Kit always creates a new Site Access Credential on Controller A and selects it automatically. Existing credentials are never silently reused.");

            ui.separator();
            ui.strong("Runtime included in the Site Kit");
            if let Some(default) = self.default_outfit_entry() {
                ui.small(format!("Appearance: {} · revision {}", default.display_name, default.revision));
            }
            if let Some(artifact) = self.active_native_client_flavor() {
                ui.label(format!(
                    "Ready: {} {} · {} · {} · {}",
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
                if let Some(error) = &self.client_flavor_import_error {
                    ui.label(
                        egui::RichText::new(format!("Runtime verification failed: {error}"))
                            .color(egui::Color32::from_rgb(185, 28, 28)),
                    );
                } else {
                    ui.label("No matching runtime has been verified yet.");
                }
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
                        self.client_flavor_import_error = None;
                        self.error = None;
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                let ready = self.site_kit_ready();
                if ui
                    .add_enabled(ready, primary_button("Create Site Kit to send..."))
                    .clicked()
                {
                    self.start_site_kit_create();
                }
                if ui
                    .add_enabled(
                        !self.invite_in_flight && !self.client_flavor_import_in_flight,
                        egui::Button::new("Cancel"),
                    )
                    .clicked()
                {
                    self.invite_open = false;
                }
                if self.invite_in_flight {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(blocker) = self.site_kit_blocker() {
                        ui.label(
                            egui::RichText::new(blocker)
                                .small()
                                .color(egui::Color32::from_rgb(100, 116, 139)),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Ready to create")
                                .small()
                                .color(egui::Color32::from_rgb(5, 150, 105)),
                        );
                    }
                });
            });
            ui.add_space(4.0);

            egui::CollapsingHeader::new("Advanced: export signed site.clew only")
                .default_open(false)
                .show(ui, |ui| {
                    ui.small("Use this only for a different target platform or an existing matching runtime. It does not create a complete Site Kit.");
                    let ready = !self.invite_site_name.trim().is_empty()
                        && (self.invite_all_filesystem || !self.invite_read_root.trim().is_empty())
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

    fn site_kit_ready(&self) -> bool {
        !self.invite_site_name.trim().is_empty()
            && (self.invite_all_filesystem || !self.invite_read_root.trim().is_empty())
            && self.active_native_client_flavor().is_some()
            && !self.invite_in_flight
            && !self.client_flavor_import_in_flight
            && self.client_flavor_import_error.is_none()
    }

    fn site_kit_blocker(&self) -> Option<&'static str> {
        if self.invite_in_flight {
            Some("Creating Site Kit...")
        } else if self.client_flavor_import_in_flight {
            Some("Verifying runtime...")
        } else if self.invite_site_name.trim().is_empty() {
            Some("Enter a Site / collaborator name")
        } else if !self.invite_all_filesystem && self.invite_read_root.trim().is_empty() {
            Some("Choose an approved folder on target B")
        } else if self.client_flavor_import_error.is_some() {
            Some("Runtime verification failed — see details above")
        } else if self.active_native_client_flavor().is_none() {
            Some("Runtime verification required")
        } else {
            None
        }
    }

    fn start_site_kit_create(&mut self) {
        if !self.site_kit_ready() {
            return;
        }
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            self.backend.site_kit_create(SiteKitCreateRequest {
                invite: self.invite_request(),
                output_dir: folder.to_string_lossy().into_owned(),
            });
            self.invite_in_flight = true;
            self.error = None;
        }
    }

    fn render_fixed_footer(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(egui::Color32::WHITE)
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(226, 232, 240),
            ))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    muted(
                        ui,
                        "Clew stays available in the tray when this window is hidden.",
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Exit Clew").clicked() {
                            self.backend.shutdown();
                        }
                        if ui.button("Hide to tray").clicked() {
                            ui.ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
                        }
                    });
                });
                ui.separator();

                let controller_ready =
                    self.status.as_ref().is_some_and(|status| status.ready) && self.error.is_none();
                let runtime = if self.client_flavor_import_in_flight {
                    "Verifying".to_owned()
                } else if self.client_flavor_import_error.is_some() {
                    "Verification failed".to_owned()
                } else if let Some(artifact) = self.active_native_client_flavor() {
                    if self.controller_config.local_acceptance_runtime() && !artifact.release_ready
                    {
                        format!("{} local acceptance", artifact.version)
                    } else {
                        format!("{} ready", artifact.version)
                    }
                } else {
                    "Unavailable".to_owned()
                };

                ui.columns(4, |columns| {
                    columns[0].vertical(|ui| {
                        ui.small("Controller");
                        ui.strong(if controller_ready {
                            "Ready"
                        } else {
                            "Starting"
                        });
                    });
                    columns[1].vertical(|ui| {
                        ui.small("Runtime");
                        ui.strong(runtime);
                    });
                    columns[2].vertical(|ui| {
                        ui.small("Site Kit");
                        ui.strong(if self.last_site_kit_path.is_some() {
                            "Created"
                        } else {
                            "Not created"
                        });
                    });
                    columns[3].vertical(|ui| {
                        ui.small("MCP");
                        ui.strong(if self.mcp_http.is_some() {
                            "Running"
                        } else {
                            "Stopped"
                        });
                    });
                });
            });
    }

    fn render_site_kit_next_steps(&self, ui: &mut egui::Ui) {
        let (Some(path), Some(scenario)) = (&self.last_site_kit_path, self.last_site_kit_scenario)
        else {
            return;
        };
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(239, 246, 255))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(191, 219, 254)))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
            ui.strong(egui::RichText::new("Site Kit ready").size(18.0));
            ui.horizontal_wrapped(|ui| {
                ui.label("Created:");
                ui.monospace(path);
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(path.clone());
                }
            });
            ui.add_space(6.0);
            if let Some(invite_id) = self.last_site_kit_invite_id {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(format!(
                        "Site Access Credential: {}",
                        site_access_credential_id(invite_id)
                    ));
                    ui.small(format!("InviteId: {invite_id}"));
                });
                ui.small("B and C should show this same Credential ID when they open the Site Kit.");
                ui.add_space(6.0);
            }
            match scenario {
                InviteScenario::DirectTarget => {
                    ui.strong("A/B setup");
                    ui.label("1. A is this Controller computer. Send this archive to target computer B.");
                    ui.label("2. On B: extract the full archive, open Clew.exe, choose “Use this computer”, and keep Clew running.");
                    ui.label("3. Back on A: wait for B to appear as a connected Target, then start MCP below.");
                }
                InviteScenario::PrivateTargetViaHelper => {
                    ui.strong("A/B/C setup");
                    ui.label("1. A is this Controller computer. Send this same archive to both target B and helper C.");
                    ui.label("2. On C: extract it, open Clew.exe, choose “Help nearby computers connect”, and keep Clew running.");
                    ui.label("3. On B: extract the same archive, open Clew.exe, choose “Use this computer”, and keep Clew running.");
                    ui.small("Strict A/B/C validation: if B itself can reach the Internet, enable “Private-network validation: require helper C for target B” before starting B. This disables direct B→A dialing so the test proves the C path.");
                    ui.label("4. Back on A: wait for C to appear as a Connection helper and B as a Target, then start MCP below.");
                    ui.small("If B keeps waiting because LAN discovery is blocked: on C use “Save Nearby Connection File...”, copy nearby-connection.clew to B, and drop it onto B's Clew window. Clew retries automatically.");
                }
            }
        });
    }

    fn render_credentials(&mut self, ui: &mut egui::Ui) {
        section_card().show(ui, |ui| {
            ui.strong(egui::RichText::new("Site Access Credentials").size(18.0));
            muted(
                ui,
                "Each Site Kit has its own enrollment credential. Select one to see only the B/C computers enrolled from that Kit.",
            );
            ui.add_space(6.0);

            if self.credentials.credentials.is_empty() {
                muted(ui, "No Site Access Credentials yet. Create a Site Kit above.");
                return;
            }

            let credentials = self.credentials.credentials.clone();
            let selected_text = self
                .selected_credential
                .and_then(|selected| {
                    credentials
                        .iter()
                        .find(|credential| credential.invite_id == selected)
                })
                .map(|credential| {
                    format!("{} · {}", credential.credential_id, credential.site_name)
                })
                .unwrap_or_else(|| "Select credential".into());
            egui::ComboBox::from_id_salt("site-access-credential-selector")
                .selected_text(selected_text)
                .width(360.0)
                .show_ui(ui, |ui| {
                    for credential in &credentials {
                        ui.selectable_value(
                            &mut self.selected_credential,
                            Some(credential.invite_id),
                            format!("{} · {}", credential.credential_id, credential.site_name),
                        );
                    }
                });

            let Some(selected) = self.selected_credential else {
                return;
            };
            let Some(credential) = credentials
                .iter()
                .find(|credential| credential.invite_id == selected)
                .cloned()
            else {
                return;
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| duration.as_millis().try_into().ok())
                .unwrap_or(0_u64);
            let expired = credential
                .expires_unix_ms
                .is_some_and(|expires| expires <= now);
            let state_label = if credential.revoked {
                "Revoked"
            } else if credential.closed {
                "Closed"
            } else if expired {
                "Expired"
            } else {
                "Open"
            };
            let claims = credential
                .max_claims
                .map(|max| format!("{}/{} enrolled", credential.claim_count, max))
                .unwrap_or_else(|| format!("{} enrolled", credential.claim_count));
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(248, 250, 252))
                .corner_radius(egui::CornerRadius::same(9))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("{} · {}", credential.credential_id, state_label));
                        ui.label(format!("· {claims}"));
                        if let Some(expires) = credential.expires_unix_ms {
                            if expires > now {
                                let hours = expires.saturating_sub(now) / (60 * 60 * 1_000);
                                ui.label(format!("· expires in {hours}h"));
                            } else {
                                ui.label("· expired");
                            }
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.small("InviteId:");
                        ui.monospace(credential.invite_id.to_string());
                        if ui.small_button("Copy InviteId").clicked() {
                            ui.ctx().copy_text(credential.invite_id.to_string());
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.small("Credential fingerprint:");
                        ui.monospace(&credential.fingerprint_sha256);
                        if ui.small_button("Copy fingerprint").clicked() {
                            ui.ctx().copy_text(credential.fingerprint_sha256.clone());
                        }
                    });
                    let mut permissions = Vec::new();
                    if credential.allow_read == Some(true) {
                        permissions.push("Read");
                    }
                    if credential.allow_write == Some(true) {
                        permissions.push("Write");
                    }
                    if credential.allow_shell == Some(true) {
                        permissions.push("Shell");
                    }
                    if credential.allow_tcp_egress == Some(true) {
                        permissions.push("TCP egress");
                    }
                    if !permissions.is_empty() {
                        ui.small(format!("Signed target permissions: {}", permissions.join(" · ")));
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if !credential.closed && !credential.revoked
                            && ui.button("Close to new devices").clicked()
                        {
                            self.backend.credential_close(credential.invite_id);
                        }
                        if !credential.revoked {
                            if self.credential_confirm
                                == Some(CredentialConfirmAction::Revoke(credential.invite_id))
                            {
                                ui.label("Revoke this credential and disconnect its enrolled devices?");
                                if ui.button("Confirm revoke").clicked() {
                                    self.backend.credential_revoke(credential.invite_id);
                                }
                                if ui.button("Cancel").clicked() {
                                    self.credential_confirm = None;
                                }
                            } else if ui.button("Revoke credential...").clicked() {
                                self.credential_confirm =
                                    Some(CredentialConfirmAction::Revoke(credential.invite_id));
                            }
                        }
                    });
                    let deletable = credential.closed || credential.revoked || expired;
                    if self.credential_confirm
                        == Some(CredentialConfirmAction::Delete(credential.invite_id))
                    {
                        ui.separator();
                        ui.label("Delete this old credential from normal history and remove its enrolled device records? Clew will retain only a revocation tombstone so the old Site Kit cannot be reused.");
                        ui.horizontal(|ui| {
                            if ui.button("Confirm delete old credential").clicked() {
                                self.backend.credential_delete(credential.invite_id);
                                if self.selected_credential == Some(credential.invite_id) {
                                    self.selected_credential = None;
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                self.credential_confirm = None;
                            }
                        });
                    } else if deletable {
                        if ui.button("Delete old credential...").clicked() {
                            self.credential_confirm =
                                Some(CredentialConfirmAction::Delete(credential.invite_id));
                        }
                    } else {
                        ui.small("Close or revoke this credential before deleting it from history.");
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
            all_filesystem: self.invite_all_filesystem,
            roots: if self.invite_all_filesystem {
                Vec::new()
            } else {
                vec![self.invite_read_root.trim().to_owned()]
            },
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

fn should_import_current_runtime(
    local_acceptance_runtime: bool,
    current_runtime_import_attempted: bool,
    has_active_native_flavor: bool,
    import_in_flight: bool,
    invite_in_flight: bool,
) -> bool {
    if import_in_flight || invite_in_flight {
        return false;
    }
    !has_active_native_flavor || (local_acceptance_runtime && !current_runtime_import_attempted)
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
    fn local_acceptance_refreshes_current_package_once_even_with_active_runtime() {
        assert!(should_import_current_runtime(
            true, false, true, false, false
        ));
        assert!(!should_import_current_runtime(
            true, true, true, false, false
        ));
        assert!(!should_import_current_runtime(
            false, false, true, false, false
        ));
        assert!(should_import_current_runtime(
            false, false, false, false, false
        ));
        assert!(!should_import_current_runtime(
            true, false, true, true, false
        ));
        assert!(!should_import_current_runtime(
            true, false, true, false, true
        ));
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
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [245.0 / 255.0, 247.0 / 255.0, 250.0 / 255.0, 1.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events(&ctx);

        if ctx.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let footer_height = 118.0;
        let footer_gap = 6.0;
        let scroll_height = (ui.available_height() - footer_height - footer_gap).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), scroll_height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("controller-main-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Clew").size(30.0).strong());
                muted(
                    ui,
                    "Controller A · private remote capability bridge for your agent",
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ready =
                    self.status.as_ref().is_some_and(|status| status.ready) && self.error.is_none();
                status_badge(
                    ui,
                    if ready {
                        "Controller ready"
                    } else {
                        "Controller starting"
                    },
                    ready,
                );
            });
        });
        ui.add_space(12.0);

        let target_online = self.devices.devices.iter().any(|device| {
            device.executable
                && device.online
                && self.selected_credential == Some(device.enrolled_via_invite_id)
        });
            let site_ready = self.last_site_kit_path.is_some() || !self.credentials.credentials.is_empty();
            let mcp_running = self.mcp_http.is_some();
            section_card().show(ui, |ui| {
                ui.columns(3, |columns| {
                    workflow_step(
                        &mut columns[0],
                        FlowGlyph::Package,
                        if site_ready {
                            FlowState::Complete
                        } else {
                            FlowState::Active
                        },
                        "1 · Site Kit",
                        if site_ready {
                            "Package ready or already used"
                        } else {
                            "Create the collaborator package"
                        },
                    );
                    workflow_step(
                        &mut columns[1],
                        FlowGlyph::Computer,
                        if target_online {
                            FlowState::Complete
                        } else if site_ready {
                            FlowState::Active
                        } else {
                            FlowState::Pending
                        },
                        "2 · Target B",
                        if target_online {
                            "Target connected"
                        } else {
                            "Open the Site Kit on B"
                        },
                    );
                    workflow_step(
                        &mut columns[2],
                        FlowGlyph::Agent,
                        if mcp_running {
                            FlowState::Complete
                        } else if target_online {
                            FlowState::Active
                        } else {
                            FlowState::Pending
                        },
                        "3 · Agent MCP",
                        if mcp_running {
                            "Agent endpoint running"
                        } else {
                            "Start after B is connected"
                        },
                    );
                });
            });
            ui.add_space(12.0);

        if let Some(error) = &self.error {
            section_card().show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("Controller unavailable: {error}"))
                        .color(egui::Color32::from_rgb(185, 28, 28)),
                );
            });
        }
        if let Some(notice) = &self.notice {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(240, 253, 250))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.label(notice);
                });
        }

        section_card().show(ui, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("1. Create Site Kit")
                    .size(18.0)
                    .strong(),
            )
            .default_open(self.credentials.credentials.is_empty())
            .show(ui, |ui| {
                if !self.invite_open {
                    self.invite_open = true;
                    self.import_current_runtime_if_available();
                }
                self.render_invite(ui);
            });
            muted(
                ui,
                "Create one package for B, or the same package for B + helper C.",
            );
        });
        ui.add_space(10.0);
        self.render_site_kit_next_steps(ui);
        ui.add_space(10.0);
        self.render_credentials(ui);

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

        ui.add_space(10.0);
        section_card().show(ui, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("2. Collaborator computers").size(18.0).strong(),
            )
            .default_open(self.selected_credential.is_some())
            .show(ui, |ui| {
                let visible_devices = self
                    .selected_credential
                    .map(|credential| {
                        self.devices
                            .devices
                            .iter()
                            .filter(|device| device.enrolled_via_invite_id == credential)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if let Some(credential) = self.selected_credential
                    && let Some(summary) = self
                        .credentials
                        .credentials
                        .iter()
                        .find(|item| item.invite_id == credential)
                {
                    muted(
                        ui,
                        &format!(
                            "Showing computers enrolled with {}. Select another credential above to review its machines.",
                            summary.credential_id
                        ),
                    );
                    ui.add_space(6.0);
                }
                if self.selected_credential.is_none() {
                    muted(ui, "Create or select a Site Access Credential to view its collaborator computers.");
                } else if visible_devices.is_empty() {
                    muted(ui, "No computers are enrolled with the selected credential yet. Start B, and C for an A/B/C setup, from the matching Site Kit.");
                } else {
                    for device in visible_devices {
                        let role = if device.executable {
                            "Target B"
                        } else if device.connector {
                            "Connection helper C"
                        } else {
                            "Site member"
                        };
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(248, 250, 252))
                            .corner_radius(egui::CornerRadius::same(9))
                            .inner_margin(egui::Margin::same(10))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    flow_icon(
                                        ui,
                                        if device.executable {
                                            FlowGlyph::Computer
                                        } else {
                                            FlowGlyph::Link
                                        },
                                        if device.online {
                                            FlowState::Complete
                                        } else {
                                            FlowState::Pending
                                        },
                                    );
                                    ui.vertical(|ui| {
                                        ui.strong(format!("{} / {}", device.site_name, device.display_name));
                                        muted(ui, role);
                                    });
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        status_badge(ui, if device.online { "Connected" } else { "Offline" }, device.online);
                                    });
                                });
                            });
                    }
                }
            });
        });

        ui.add_space(10.0);
        section_card().show(ui, |ui| {
            egui::CollapsingHeader::new(
                egui::RichText::new("3. Agent access (MCP)")
                    .size(18.0)
                    .strong(),
            )
            .default_open(
                self.devices
                    .devices
                    .iter()
                    .any(|device| device.executable && device.online),
            )
            .show(ui, |ui| self.render_mcp(ui));
        });

        ui.add_space(10.0);
        section_card().show(ui, |ui| {
            egui::CollapsingHeader::new("Advanced")
                .default_open(false)
                .show(ui, |ui| {
                    let mut studio_actions = Vec::new();
                    egui::CollapsingHeader::new("Outfit Studio")
                        .default_open(false)
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
                });
        });

        ui.add_space(12.0);
                    });
            },
        );
        ui.add_space(footer_gap);
        self.render_fixed_footer(ui);
    }
}
