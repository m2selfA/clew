use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_host::{
    KEY_AWAITING_ENROLLMENT, KEY_EXIT_AND_DISCONNECT, KEY_HELPER_READY, KEY_HIDE_TO_TRAY,
    KEY_MISSING_INVITE_BODY, KEY_MISSING_INVITE_TITLE, KEY_READY, KEY_TRAY_CONNECTED,
    KEY_TRAY_EXIT, KEY_TRAY_SHOW, OutfitAssetRef, OutfitPreset, OutfitProfile,
};
use clew_runtime::{
    OutfitAssetInfo, OutfitAssetList, OutfitAssetPreviewResponse, OutfitCloneRequest,
    OutfitCreateRequest, OutfitEditPatch, OutfitList, OutfitSetAssetRequest, OutfitUpdateRequest,
};
use eframe::egui;

pub enum StudioAction {
    SelectOutfit(String),
    Create(OutfitCreateRequest),
    Clone(OutfitCloneRequest),
    Update(OutfitUpdateRequest),
    SetDefault(String),
    ImportAsset,
    SetAsset(OutfitSetAssetRequest),
    PreviewAsset(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OutfitDraft {
    display_name: String,
    app_display_name: String,
    window_title: String,
    primary_color: String,
    ready_text: String,
    awaiting_enrollment_text: String,
    missing_invite_title: String,
    missing_invite_body: String,
    start_here_title: String,
    start_here_body: String,
    chat_message_template: String,
}

impl OutfitDraft {
    fn from_profile(profile: &OutfitProfile) -> Self {
        let locale = &profile.strings.locale_default;
        Self {
            display_name: profile.display_name.clone(),
            app_display_name: profile.identity.app_display_name.clone(),
            window_title: profile.identity.window_title.clone(),
            primary_color: profile.visuals.primary_color.clone(),
            ready_text: profile.resolve_resource(locale, KEY_READY),
            awaiting_enrollment_text: profile.resolve_resource(locale, KEY_AWAITING_ENROLLMENT),
            missing_invite_title: profile.resolve_resource(locale, KEY_MISSING_INVITE_TITLE),
            missing_invite_body: profile.resolve_resource(locale, KEY_MISSING_INVITE_BODY),
            start_here_title: profile.distribution_copy.start_here_title.clone(),
            start_here_body: profile.distribution_copy.start_here_body.clone(),
            chat_message_template: profile.distribution_copy.chat_message_template.clone(),
        }
    }

    fn patch(&self) -> OutfitEditPatch {
        OutfitEditPatch {
            display_name: self.display_name.clone(),
            app_display_name: self.app_display_name.clone(),
            window_title: self.window_title.clone(),
            primary_color: self.primary_color.clone(),
            ready_text: self.ready_text.clone(),
            awaiting_enrollment_text: self.awaiting_enrollment_text.clone(),
            missing_invite_title: self.missing_invite_title.clone(),
            missing_invite_body: self.missing_invite_body.clone(),
            start_here_title: self.start_here_title.clone(),
            start_here_body: self.start_here_body.clone(),
            chat_message_template: self.chat_message_template.clone(),
        }
    }

    fn preview_profile(&self, saved: &OutfitProfile) -> OutfitProfile {
        let mut profile = saved.clone();
        profile.display_name.clone_from(&self.display_name);
        profile
            .identity
            .app_display_name
            .clone_from(&self.app_display_name);
        profile.identity.window_title.clone_from(&self.window_title);
        profile
            .visuals
            .primary_color
            .clone_from(&self.primary_color);
        let locale = profile.strings.locale_default.clone();
        let resources = profile
            .strings
            .resources_by_locale
            .entry(locale)
            .or_default();
        resources.insert(KEY_READY.into(), self.ready_text.clone());
        resources.insert(
            KEY_AWAITING_ENROLLMENT.into(),
            self.awaiting_enrollment_text.clone(),
        );
        resources.insert(
            KEY_MISSING_INVITE_TITLE.into(),
            self.missing_invite_title.clone(),
        );
        resources.insert(
            KEY_MISSING_INVITE_BODY.into(),
            self.missing_invite_body.clone(),
        );
        profile
            .distribution_copy
            .start_here_title
            .clone_from(&self.start_here_title);
        profile
            .distribution_copy
            .start_here_body
            .clone_from(&self.start_here_body);
        profile
            .distribution_copy
            .chat_message_template
            .clone_from(&self.chat_message_template);
        profile
    }
}

pub struct StudioState {
    outfits: OutfitList,
    assets: OutfitAssetList,
    selected_id: Option<String>,
    profile: Option<OutfitProfile>,
    draft: Option<OutfitDraft>,
    new_id: String,
    new_name: String,
    new_preset: OutfitPreset,
    clone_id: String,
    clone_name: String,
    selected_asset_id: Option<String>,
    textures: BTreeMap<String, egui::TextureHandle>,
    preview_loading: BTreeSet<String>,
    profile_loading: bool,
    busy: bool,
}

impl StudioState {
    pub fn new() -> Self {
        Self {
            outfits: OutfitList {
                entries: Vec::new(),
                default_outfit_id: OutfitPreset::ClewOriginal.id().into(),
                recent_outfit_id: None,
            },
            assets: OutfitAssetList { assets: Vec::new() },
            selected_id: None,
            profile: None,
            draft: None,
            new_id: "my-outfit".into(),
            new_name: "My Outfit".into(),
            new_preset: OutfitPreset::ResearchLab,
            clone_id: String::new(),
            clone_name: String::new(),
            selected_asset_id: None,
            textures: BTreeMap::new(),
            preview_loading: BTreeSet::new(),
            profile_loading: false,
            busy: false,
        }
    }

    pub fn set_catalogs(
        &mut self,
        outfits: OutfitList,
        assets: OutfitAssetList,
    ) -> Option<StudioAction> {
        self.outfits = outfits;
        self.assets = assets;
        if let Some(selected) = self.selected_id.as_deref()
            && !self
                .outfits
                .entries
                .iter()
                .any(|entry| entry.outfit_id == selected)
        {
            self.selected_id = None;
            self.profile = None;
            self.draft = None;
            self.profile_loading = false;
        }

        if self.selected_id.is_none() {
            let preferred = self
                .outfits
                .recent_outfit_id
                .clone()
                .unwrap_or_else(|| self.outfits.default_outfit_id.clone());
            if self
                .outfits
                .entries
                .iter()
                .any(|entry| entry.outfit_id == preferred)
            {
                return Some(self.begin_select(preferred));
            }
        }

        if !self.profile_loading
            && let (Some(selected), Some(profile)) =
                (self.selected_id.as_deref(), self.profile.as_ref())
            && let Some(entry) = self
                .outfits
                .entries
                .iter()
                .find(|entry| entry.outfit_id == selected)
            && entry.revision != profile.revision
        {
            return Some(self.begin_select(selected.to_owned()));
        }
        None
    }

    pub fn accept_profile(&mut self, profile: OutfitProfile) -> Vec<StudioAction> {
        self.profile_loading = false;
        self.busy = false;
        self.selected_id = Some(profile.outfit_id.clone());
        self.clone_id = format!("{}-copy", profile.outfit_id);
        self.clone_name = format!("{} Copy", profile.display_name);
        self.draft = Some(OutfitDraft::from_profile(&profile));
        self.profile = Some(profile);
        self.refresh_referenced_previews()
    }

    pub fn accept_asset_import(&mut self, info: OutfitAssetInfo) -> Vec<StudioAction> {
        self.busy = false;
        self.selected_asset_id = Some(info.asset_id.clone());
        match self
            .assets
            .assets
            .iter_mut()
            .find(|existing| existing.asset_id == info.asset_id)
        {
            Some(existing) => *existing = info.clone(),
            None => self.assets.assets.push(info.clone()),
        }
        self.assets
            .assets
            .sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
        self.request_preview_if_needed(&info.asset_id)
            .into_iter()
            .collect()
    }

    pub fn accept_preview(
        &mut self,
        ctx: &egui::Context,
        preview: OutfitAssetPreviewResponse,
    ) -> Result<(), String> {
        self.preview_loading.remove(&preview.asset_id);
        let rgba = BASE64_STANDARD
            .decode(preview.rgba_base64.as_bytes())
            .map_err(|error| format!("asset preview base64 is invalid: {error}"))?;
        let width = usize::try_from(preview.width)
            .map_err(|_| "asset preview width does not fit this platform".to_string())?;
        let height = usize::try_from(preview.height)
            .map_err(|_| "asset preview height does not fit this platform".to_string())?;
        let expected = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "asset preview dimensions overflow".to_string())?;
        if width == 0 || height == 0 || rgba.len() != expected {
            return Err("asset preview dimensions do not match RGBA payload".into());
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba);
        let texture = ctx.load_texture(
            format!("clew-outfit-{}", preview.asset_id),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.textures.insert(preview.asset_id, texture);
        Ok(())
    }

    pub fn accept_default_change(&mut self) {
        self.busy = false;
    }

    pub fn accept_error(&mut self) {
        self.busy = false;
        self.profile_loading = false;
        self.preview_loading.clear();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<StudioAction> {
        let mut actions = Vec::new();
        ui.heading("Outfit Studio");
        ui.label(
            "Build branded Site Kits without changing enrollment, permissions, or transport security.",
        );
        ui.add_space(6.0);

        ui.collapsing("Create from preset", |ui| {
            ui.horizontal(|ui| {
                ui.label("Preset");
                egui::ComboBox::from_id_salt("studio-new-preset")
                    .selected_text(preset_label(self.new_preset))
                    .show_ui(ui, |ui| {
                        for preset in OutfitPreset::ALL {
                            ui.selectable_value(&mut self.new_preset, preset, preset_label(preset));
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label("Outfit ID");
                ui.text_edit_singleline(&mut self.new_id);
            });
            ui.horizontal(|ui| {
                ui.label("Display name");
                ui.text_edit_singleline(&mut self.new_name);
            });
            if ui
                .add_enabled(
                    !self.busy
                        && !self.new_id.trim().is_empty()
                        && !self.new_name.trim().is_empty(),
                    egui::Button::new("Create Outfit"),
                )
                .clicked()
            {
                self.busy = true;
                actions.push(StudioAction::Create(OutfitCreateRequest {
                    outfit_id: self.new_id.trim().to_owned(),
                    display_name: self.new_name.trim().to_owned(),
                    preset: self.new_preset,
                }));
            }
        });

        ui.separator();
        ui.label("Library");
        let entries = self.outfits.entries.clone();
        egui::ScrollArea::horizontal()
            .id_salt("studio-outfit-library")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for entry in entries {
                        let selected = self.selected_id.as_deref() == Some(&entry.outfit_id);
                        let mut label = format!("{} · r{}", entry.display_name, entry.revision);
                        if entry.is_default {
                            label.push_str(" · default");
                        }
                        if entry.built_in {
                            label.push_str(" · built-in");
                        }
                        if ui.selectable_label(selected, label).clicked() && !selected {
                            actions.push(self.begin_select(entry.outfit_id));
                        }
                    }
                });
            });

        let Some(saved) = self.profile.clone() else {
            ui.add_space(12.0);
            ui.label(if self.profile_loading {
                "Loading Outfit..."
            } else {
                "Select an Outfit to edit and preview it."
            });
            return actions;
        };
        let built_in = self.selected_is_builtin();
        let is_default = self.outfits.default_outfit_id == saved.outfit_id;

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(format!(
                "{} · revision {}",
                saved.display_name, saved.revision
            ));
            if let Ok(key) = saved.build_cache_key() {
                ui.small(format!("build key {}...", &key[..24]));
            }
            if !is_default
                && ui
                    .add_enabled(!self.busy, egui::Button::new("Set as default"))
                    .clicked()
            {
                self.busy = true;
                actions.push(StudioAction::SetDefault(saved.outfit_id.clone()));
            }
        });

        ui.collapsing("Clone selected Outfit", |ui| {
            ui.horizontal(|ui| {
                ui.label("New ID");
                ui.text_edit_singleline(&mut self.clone_id);
            });
            ui.horizontal(|ui| {
                ui.label("Display name");
                ui.text_edit_singleline(&mut self.clone_name);
            });
            if ui
                .add_enabled(
                    !self.busy
                        && !self.clone_id.trim().is_empty()
                        && !self.clone_name.trim().is_empty(),
                    egui::Button::new("Clone to editable Outfit"),
                )
                .clicked()
            {
                self.busy = true;
                actions.push(StudioAction::Clone(OutfitCloneRequest {
                    source_id: saved.outfit_id.clone(),
                    outfit_id: self.clone_id.trim().to_owned(),
                    display_name: self.clone_name.trim().to_owned(),
                }));
            }
        });

        let draft = self
            .draft
            .get_or_insert_with(|| OutfitDraft::from_profile(&saved));
        if built_in {
            ui.label("Built-in presets are read-only. Clone this Outfit before editing.");
        } else {
            ui.collapsing("Identity and copy", |ui| {
                editor_row(ui, "Library name", &mut draft.display_name);
                editor_row(ui, "App name", &mut draft.app_display_name);
                editor_row(ui, "Window title", &mut draft.window_title);
                editor_row(ui, "Primary color", &mut draft.primary_color);
                ui.label("Ready message");
                ui.text_edit_singleline(&mut draft.ready_text);
                ui.label("Connecting message");
                ui.text_edit_singleline(&mut draft.awaiting_enrollment_text);
                ui.label("Missing-invite title");
                ui.text_edit_singleline(&mut draft.missing_invite_title);
                ui.label("Missing-invite body");
                ui.add(egui::TextEdit::multiline(&mut draft.missing_invite_body).desired_rows(2));
                ui.label("Site Kit start title");
                ui.text_edit_singleline(&mut draft.start_here_title);
                ui.label("Site Kit start body");
                ui.add(egui::TextEdit::multiline(&mut draft.start_here_body).desired_rows(2));
                ui.label("Chat message template");
                ui.add(egui::TextEdit::multiline(&mut draft.chat_message_template).desired_rows(2));
            });
            let dirty = *draft != OutfitDraft::from_profile(&saved);
            if ui
                .add_enabled(
                    dirty && !self.busy,
                    egui::Button::new(if self.busy {
                        "Saving..."
                    } else {
                        "Apply changes"
                    }),
                )
                .clicked()
            {
                self.busy = true;
                actions.push(StudioAction::Update(OutfitUpdateRequest {
                    outfit_id: saved.outfit_id.clone(),
                    patch: draft.patch(),
                }));
            }
        }

        ui.separator();
        ui.collapsing("Visual assets", |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Import PNG/SVG..."))
                    .clicked()
                {
                    actions.push(StudioAction::ImportAsset);
                }
                ui.label(format!("{} imported asset(s)", self.assets.assets.len()));
            });

            let assets = self.assets.assets.clone();
            egui::ScrollArea::vertical()
                .id_salt("studio-assets")
                .max_height(180.0)
                .show(ui, |ui| {
                    for asset in assets {
                        ui.horizontal_wrapped(|ui| {
                            let selected =
                                self.selected_asset_id.as_deref() == Some(asset.asset_id.as_str());
                            if ui
                                .selectable_label(
                                    selected,
                                    format!(
                                        "{} · {}×{} · {} B",
                                        asset_format_label(&asset),
                                        asset.width,
                                        asset.height,
                                        asset.byte_len
                                    ),
                                )
                                .clicked()
                            {
                                self.selected_asset_id = Some(asset.asset_id.clone());
                                if let Some(action) =
                                    self.request_preview_if_needed(&asset.asset_id)
                                {
                                    actions.push(action);
                                }
                            }
                            ui.small(short_asset_id(&asset.asset_id));
                            if !built_in {
                                for (label, slot) in [
                                    ("App icon", "app-icon"),
                                    ("Tray", "tray-icon"),
                                    ("Logo", "logo"),
                                    ("Key visual", "key-visual"),
                                ] {
                                    if ui
                                        .add_enabled(!self.busy, egui::Button::new(label))
                                        .clicked()
                                    {
                                        self.busy = true;
                                        actions.push(StudioAction::SetAsset(
                                            OutfitSetAssetRequest {
                                                outfit_id: saved.outfit_id.clone(),
                                                slot: slot.into(),
                                                asset_id: asset.asset_id.clone(),
                                            },
                                        ));
                                    }
                                }
                            }
                        });
                    }
                });

            if let Some(asset_id) = self.selected_asset_id.as_deref() {
                ui.label(format!("Selected asset: {}", short_asset_id(asset_id)));
                if let Some(texture) = self.textures.get(asset_id) {
                    let size = fit_texture(texture, 160.0);
                    ui.add(egui::Image::new((texture.id(), size)));
                } else if self.preview_loading.contains(asset_id) {
                    ui.label("Rendering preview...");
                }
            }
        });

        ui.separator();
        ui.collapsing("Live preview", |ui| {
            let preview_profile = self
                .draft
                .as_ref()
                .map(|draft| draft.preview_profile(&saved))
                .unwrap_or_else(|| saved.clone());
            self.live_preview(ui, &preview_profile);
        });
        actions
    }

    fn begin_select(&mut self, outfit_id: String) -> StudioAction {
        self.selected_id = Some(outfit_id.clone());
        self.profile = None;
        self.draft = None;
        self.profile_loading = true;
        self.preview_loading.clear();
        self.textures.clear();
        StudioAction::SelectOutfit(outfit_id)
    }

    fn selected_is_builtin(&self) -> bool {
        self.selected_id.as_deref().is_some_and(|selected| {
            self.outfits
                .entries
                .iter()
                .find(|entry| entry.outfit_id == selected)
                .is_some_and(|entry| entry.built_in)
        })
    }

    fn refresh_referenced_previews(&mut self) -> Vec<StudioAction> {
        let referenced = self
            .profile
            .as_ref()
            .map(OutfitProfile::imported_asset_ids)
            .unwrap_or_default();
        let mut keep = referenced.iter().cloned().collect::<BTreeSet<_>>();
        if let Some(selected) = self.selected_asset_id.clone() {
            keep.insert(selected);
        }
        self.textures.retain(|asset_id, _| keep.contains(asset_id));
        self.preview_loading
            .retain(|asset_id| keep.contains(asset_id));
        referenced
            .into_iter()
            .filter_map(|asset_id| self.request_preview_if_needed(&asset_id))
            .collect()
    }

    fn request_preview_if_needed(&mut self, asset_id: &str) -> Option<StudioAction> {
        if self.textures.contains_key(asset_id) || self.preview_loading.contains(asset_id) {
            return None;
        }
        self.preview_loading.insert(asset_id.to_owned());
        Some(StudioAction::PreviewAsset(asset_id.to_owned()))
    }

    fn live_preview(&self, ui: &mut egui::Ui, profile: &OutfitProfile) {
        let locale = &profile.strings.locale_default;
        let primary = parse_color(&profile.visuals.primary_color)
            .unwrap_or_else(|| ui.visuals().selection.bg_fill);
        ui.columns(2, |columns| {
            preview_frame(&mut columns[0], "Main window", |ui| {
                accent(ui, primary);
                ui.strong(&profile.identity.window_title);
                if let Some(asset) = profile.visuals.logo.as_ref() {
                    self.show_asset_ref(ui, asset, 72.0);
                }
                ui.label(profile.resolve_resource(locale, KEY_READY));
                ui.horizontal(|ui| {
                    ui.small(profile.resolve_resource(locale, KEY_HIDE_TO_TRAY));
                    ui.small(profile.resolve_resource(locale, KEY_EXIT_AND_DISCONNECT));
                });
            });
            preview_frame(&mut columns[1], "Helper", |ui| {
                accent(ui, primary);
                ui.strong(
                    profile
                        .identity
                        .helper_window_title
                        .clone()
                        .unwrap_or_else(|| profile.resolve_resource(locale, KEY_HELPER_READY)),
                );
                ui.label(profile.resolve_resource(locale, KEY_AWAITING_ENROLLMENT));
                if let Some(asset) = profile.visuals.key_visual.as_ref() {
                    self.show_asset_ref(ui, asset, 72.0);
                }
            });
        });
        ui.add_space(6.0);
        ui.columns(2, |columns| {
            preview_frame(&mut columns[0], "Tray", |ui| {
                ui.horizontal(|ui| {
                    let tray_asset = profile
                        .visuals
                        .tray_icon_base
                        .as_ref()
                        .unwrap_or(&profile.visuals.app_icon);
                    self.show_asset_ref(ui, tray_asset, 28.0);
                    ui.vertical(|ui| {
                        ui.strong(&profile.identity.app_display_name);
                        ui.small(profile.resolve_resource(locale, KEY_TRAY_CONNECTED));
                    });
                });
                ui.small(format!(
                    "{} · {}",
                    profile.resolve_resource(locale, KEY_TRAY_SHOW),
                    profile.resolve_resource(locale, KEY_TRAY_EXIT)
                ));
            });
            preview_frame(&mut columns[1], "Site Kit", |ui| {
                accent(ui, primary);
                ui.strong(&profile.distribution_copy.start_here_title);
                ui.label(&profile.distribution_copy.start_here_body);
                ui.monospace("site.clew");
                ui.small(format!(
                    "{} · revision {}",
                    profile.outfit_id, profile.revision
                ));
            });
        });
    }

    fn show_asset_ref(&self, ui: &mut egui::Ui, asset: &OutfitAssetRef, max_edge: f32) {
        match asset {
            OutfitAssetRef::Imported { asset_id } => {
                if let Some(texture) = self.textures.get(asset_id) {
                    ui.add(egui::Image::new((
                        texture.id(),
                        fit_texture(texture, max_edge),
                    )));
                } else {
                    ui.small(format!("Imported asset {}", short_asset_id(asset_id)));
                }
            }
            OutfitAssetRef::BuiltIn { key } => {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(max_edge.min(42.0), max_edge.min(42.0)),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .rect_filled(rect, 6.0, ui.visuals().selection.bg_fill);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "C",
                    egui::FontId::proportional(18.0),
                    ui.visuals().strong_text_color(),
                );
                ui.small(format!("Built-in: {key}"));
            }
        }
    }
}

fn editor_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value);
    });
}

fn preview_frame(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.small(title);
        ui.separator();
        add(ui);
    });
}

fn accent(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
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

fn fit_texture(texture: &egui::TextureHandle, max_edge: f32) -> egui::Vec2 {
    let size = texture.size_vec2();
    let scale = (max_edge / size.x.max(size.y)).min(1.0);
    size * scale
}

fn preset_label(preset: OutfitPreset) -> &'static str {
    match preset {
        OutfitPreset::ClewOriginal => "Clew Original",
        OutfitPreset::ResearchLab => "Research Lab",
        OutfitPreset::FriendlyMinimal => "Friendly Minimal",
        OutfitPreset::InstitutionClean => "Institution Clean",
    }
}

fn asset_format_label(asset: &OutfitAssetInfo) -> &'static str {
    match asset.format {
        clew_runtime::OutfitAssetFormat::Png => "PNG",
        clew_runtime::OutfitAssetFormat::Svg => "SVG",
    }
}

fn short_asset_id(asset_id: &str) -> String {
    if asset_id.len() <= 24 {
        asset_id.to_owned()
    } else {
        format!("{}...{}", &asset_id[..15], &asset_id[asset_id.len() - 6..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_preview_tracks_default_locale_without_mutating_saved_profile() {
        let saved = OutfitProfile::preset(OutfitPreset::ResearchLab);
        let mut draft = OutfitDraft::from_profile(&saved);
        draft.window_title = "Preview Window".into();
        draft.ready_text = "Preview ready.".into();
        let preview = draft.preview_profile(&saved);
        assert_eq!(preview.identity.window_title, "Preview Window");
        assert_eq!(
            preview.resolve_resource(&preview.strings.locale_default, KEY_READY),
            "Preview ready."
        );
        assert_ne!(preview.identity.window_title, saved.identity.window_title);
    }

    #[test]
    fn color_parser_is_strict_rgb_hex() {
        assert_eq!(
            parse_color("#2684FF"),
            Some(egui::Color32::from_rgb(0x26, 0x84, 0xFF))
        );
        assert!(parse_color("2684FF").is_none());
        assert!(parse_color("#fff").is_none());
    }
}
