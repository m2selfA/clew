use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const OUTFIT_SCHEMA_VERSION: u32 = 1;
pub const MAX_OUTFIT_ENCODED_BYTES: usize = 256 * 1024;
pub const MAX_OUTFIT_ASSET_BYTES: usize = 512 * 1024;
pub const OUTFIT_BUILD_SPEC_SCHEMA_VERSION: u32 = 1;
pub const MAX_OUTFIT_BUILD_SPEC_BYTES: usize = 512 * 1024;
const OUTFIT_BUILD_CACHE_DOMAIN: &[u8] = b"clew/outfit-build-cache/v1\0";
const MAX_ID_BYTES: usize = 64;
const MAX_NAME_BYTES: usize = 96;
const MAX_TEMPLATE_BYTES: usize = 160;
const MAX_CONTACT_BYTES: usize = 256;
const MAX_COPY_BYTES: usize = 4096;
const MAX_LOCALES: usize = 8;
const MAX_RESOURCES_PER_LOCALE: usize = 128;
const MAX_RESOURCE_KEY_BYTES: usize = 96;
const MAX_RESOURCE_VALUE_BYTES: usize = 4096;

pub const KEY_READY: &str = "app.ready";
pub const KEY_AWAITING_ENROLLMENT: &str = "app.awaiting_enrollment";
pub const KEY_MISSING_INVITE_TITLE: &str = "invite.missing_title";
pub const KEY_MISSING_INVITE_BODY: &str = "invite.missing_body";
pub const KEY_EXTRACT_FIRST: &str = "site.extract_first_body";
pub const KEY_CHOOSE_INVITE: &str = "invite.choose_file";
pub const KEY_HIDE_TO_TRAY: &str = "button.hide_to_tray";
pub const KEY_EXIT_AND_DISCONNECT: &str = "button.exit_disconnect";
pub const KEY_HELPER_READY: &str = "app.helper_ready_title";
pub const KEY_TRAY_CONNECTED: &str = "tray.connected";
pub const KEY_TRAY_RECONNECTING: &str = "tray.reconnecting";
pub const KEY_TRAY_SHOW: &str = "tray.show";
pub const KEY_TRAY_EXIT: &str = "tray.exit_disconnect";

const REQUIRED_RUNTIME_KEYS: &[&str] = &[
    KEY_READY,
    KEY_AWAITING_ENROLLMENT,
    KEY_MISSING_INVITE_TITLE,
    KEY_MISSING_INVITE_BODY,
    KEY_EXTRACT_FIRST,
    KEY_CHOOSE_INVITE,
    KEY_HIDE_TO_TRAY,
    KEY_EXIT_AND_DISCONNECT,
    KEY_HELPER_READY,
    KEY_TRAY_CONNECTED,
    KEY_TRAY_RECONNECTING,
    KEY_TRAY_SHOW,
    KEY_TRAY_EXIT,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutfitPreset {
    ClewOriginal,
    ResearchLab,
    FriendlyMinimal,
    InstitutionClean,
}

impl OutfitPreset {
    pub const ALL: [Self; 4] = [
        Self::ClewOriginal,
        Self::ResearchLab,
        Self::FriendlyMinimal,
        Self::InstitutionClean,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ClewOriginal => "clew-original",
            Self::ResearchLab => "research-lab",
            Self::FriendlyMinimal => "friendly-minimal",
            Self::InstitutionClean => "institution-clean",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::ClewOriginal => "Clew Original",
            Self::ResearchLab => "Research Lab",
            Self::FriendlyMinimal => "Friendly Minimal",
            Self::InstitutionClean => "Institution Clean",
        }
    }

    #[must_use]
    pub fn profile(self) -> OutfitProfile {
        preset_profile(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceStyle {
    Light,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitIdentity {
    pub app_display_name: String,
    pub window_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher_label: Option<String>,
    pub artifact_name_template: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum OutfitAssetRef {
    BuiltIn { key: String },
    Imported { asset_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitVisuals {
    pub app_icon: OutfitAssetRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray_icon_base: Option<OutfitAssetRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<OutfitAssetRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_visual: Option<OutfitAssetRef>,
    pub primary_color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_color: Option<String>,
    pub surface_style: SurfaceStyle,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitStrings {
    pub locale_default: String,
    pub locale_fallback: String,
    pub resources_by_locale: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitDistributionCopy {
    pub start_here_title: String,
    pub start_here_body: String,
    pub chat_message_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_contact: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitProfile {
    pub schema_version: u32,
    pub outfit_id: String,
    pub revision: u32,
    pub display_name: String,
    pub base_preset: OutfitPreset,
    pub identity: OutfitIdentity,
    pub visuals: OutfitVisuals,
    pub strings: OutfitStrings,
    pub distribution_copy: OutfitDistributionCopy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitBuildAsset {
    pub asset_id: String,
    pub relative_path: String,
    pub byte_len: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitBuildSpec {
    pub schema_version: u32,
    pub profile: OutfitProfile,
    pub build_cache_key: String,
    pub assets: Vec<OutfitBuildAsset>,
}

impl OutfitBuildSpec {
    pub fn new(
        profile: OutfitProfile,
        mut assets: Vec<OutfitBuildAsset>,
    ) -> Result<Self, OutfitError> {
        profile.validate()?;
        assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
        let build_cache_key = profile.build_cache_key()?;
        let spec = Self {
            schema_version: OUTFIT_BUILD_SPEC_SCHEMA_VERSION,
            profile,
            build_cache_key,
            assets,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), OutfitError> {
        if self.schema_version != OUTFIT_BUILD_SPEC_SCHEMA_VERSION {
            return Err(OutfitError::UnsupportedBuildSpecSchema(self.schema_version));
        }
        self.profile.validate()?;
        if self.build_cache_key != self.profile.build_cache_key()? {
            return Err(OutfitError::BuildCacheKeyMismatch);
        }
        let expected_ids = self.profile.imported_asset_ids();
        let actual_ids = self
            .assets
            .iter()
            .map(|asset| asset.asset_id.clone())
            .collect::<Vec<_>>();
        if actual_ids != expected_ids {
            return Err(OutfitError::BuildAssetSetMismatch);
        }
        let mut previous: Option<&str> = None;
        for asset in &self.assets {
            validate_asset(&OutfitAssetRef::Imported {
                asset_id: asset.asset_id.clone(),
            })?;
            if previous.is_some_and(|value| value >= asset.asset_id.as_str()) {
                return Err(OutfitError::BuildAssetSetMismatch);
            }
            previous = Some(asset.asset_id.as_str());
            if asset.byte_len == 0 || asset.byte_len as usize > MAX_OUTFIT_ASSET_BYTES {
                return Err(OutfitError::InvalidBuildAssetLength {
                    asset_id: asset.asset_id.clone(),
                    byte_len: asset.byte_len,
                });
            }
            let expected_prefix = format!("assets/{}.", asset.asset_id);
            if !asset.relative_path.starts_with(&expected_prefix)
                || !(asset.relative_path.ends_with(".png") || asset.relative_path.ends_with(".svg"))
                || asset.relative_path[expected_prefix.len()..].contains('/')
                || asset.relative_path.contains('\\')
            {
                return Err(OutfitError::InvalidBuildAssetPath(
                    asset.relative_path.clone(),
                ));
            }
        }
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_OUTFIT_BUILD_SPEC_BYTES {
            return Err(OutfitError::BuildSpecTooLarge(encoded.len()));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, OutfitError> {
        self.validate()?;
        let encoded = serde_json::to_vec_pretty(self)?;
        if encoded.len() > MAX_OUTFIT_BUILD_SPEC_BYTES {
            return Err(OutfitError::BuildSpecTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, OutfitError> {
        if bytes.len() > MAX_OUTFIT_BUILD_SPEC_BYTES {
            return Err(OutfitError::BuildSpecTooLarge(bytes.len()));
        }
        let spec: Self = serde_json::from_slice(bytes)?;
        spec.validate()?;
        Ok(spec)
    }
}

impl OutfitProfile {
    #[must_use]
    pub fn preset(preset: OutfitPreset) -> Self {
        preset.profile()
    }

    pub fn validate(&self) -> Result<(), OutfitError> {
        if self.schema_version != OUTFIT_SCHEMA_VERSION {
            return Err(OutfitError::UnsupportedSchema(self.schema_version));
        }
        if self.revision == 0 {
            return Err(OutfitError::InvalidRevision);
        }
        validate_id(&self.outfit_id)?;
        validate_text("display_name", &self.display_name, MAX_NAME_BYTES)?;
        validate_text(
            "identity.app_display_name",
            &self.identity.app_display_name,
            MAX_NAME_BYTES,
        )?;
        validate_text(
            "identity.window_title",
            &self.identity.window_title,
            MAX_NAME_BYTES,
        )?;
        if let Some(value) = &self.identity.helper_window_title {
            validate_text("identity.helper_window_title", value, MAX_NAME_BYTES)?;
        }
        if let Some(value) = &self.identity.publisher_label {
            validate_text("identity.publisher_label", value, MAX_NAME_BYTES)?;
        }
        validate_text(
            "identity.artifact_name_template",
            &self.identity.artifact_name_template,
            MAX_TEMPLATE_BYTES,
        )?;
        validate_asset(&self.visuals.app_icon)?;
        if let Some(asset) = &self.visuals.tray_icon_base {
            validate_asset(asset)?;
        }
        if let Some(asset) = &self.visuals.logo {
            validate_asset(asset)?;
        }
        if let Some(asset) = &self.visuals.key_visual {
            validate_asset(asset)?;
        }
        validate_color(&self.visuals.primary_color)?;
        if let Some(color) = &self.visuals.secondary_color {
            validate_color(color)?;
        }
        validate_locale(&self.strings.locale_default)?;
        validate_locale(&self.strings.locale_fallback)?;
        if self.strings.resources_by_locale.len() > MAX_LOCALES {
            return Err(OutfitError::TooManyLocales(
                self.strings.resources_by_locale.len(),
            ));
        }
        for (locale, resources) in &self.strings.resources_by_locale {
            validate_locale(locale)?;
            if resources.len() > MAX_RESOURCES_PER_LOCALE {
                return Err(OutfitError::TooManyResources {
                    locale: locale.clone(),
                    count: resources.len(),
                });
            }
            for (key, value) in resources {
                validate_resource_key(key)?;
                validate_text("string resource", value, MAX_RESOURCE_VALUE_BYTES)?;
            }
        }
        let english = builtin_english_resources();
        for key in REQUIRED_RUNTIME_KEYS {
            if !english.contains_key(*key) {
                return Err(OutfitError::MissingBuiltInFallback((*key).into()));
            }
        }
        validate_text(
            "distribution_copy.start_here_title",
            &self.distribution_copy.start_here_title,
            MAX_NAME_BYTES,
        )?;
        validate_text(
            "distribution_copy.start_here_body",
            &self.distribution_copy.start_here_body,
            MAX_COPY_BYTES,
        )?;
        validate_text(
            "distribution_copy.chat_message_template",
            &self.distribution_copy.chat_message_template,
            MAX_COPY_BYTES,
        )?;
        if let Some(contact) = &self.distribution_copy.support_contact {
            validate_text(
                "distribution_copy.support_contact",
                contact,
                MAX_CONTACT_BYTES,
            )?;
        }
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_OUTFIT_ENCODED_BYTES {
            return Err(OutfitError::EncodedTooLarge(encoded.len()));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, OutfitError> {
        self.validate()?;
        let encoded = serde_json::to_vec_pretty(self)?;
        if encoded.len() > MAX_OUTFIT_ENCODED_BYTES {
            return Err(OutfitError::EncodedTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn build_cache_key(&self) -> Result<String, OutfitError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_OUTFIT_ENCODED_BYTES {
            return Err(OutfitError::EncodedTooLarge(encoded.len()));
        }
        let mut hasher = Sha256::new();
        hasher.update(OUTFIT_BUILD_CACHE_DOMAIN);
        hasher.update(encoded);
        let digest = hasher.finalize();
        let mut key = String::from("outfit-v1-");
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(key, "{byte:02x}");
        }
        Ok(key)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, OutfitError> {
        if bytes.len() > MAX_OUTFIT_ENCODED_BYTES {
            return Err(OutfitError::EncodedTooLarge(bytes.len()));
        }
        let profile: Self = serde_json::from_slice(bytes)?;
        profile.validate()?;
        Ok(profile)
    }

    #[must_use]
    pub fn resolve_resource(&self, requested_locale: &str, key: &str) -> String {
        let requested = self.strings.resources_by_locale.get(requested_locale);
        let default = self
            .strings
            .resources_by_locale
            .get(&self.strings.locale_default);
        let fallback = self
            .strings
            .resources_by_locale
            .get(&self.strings.locale_fallback);
        requested
            .and_then(|resources| resources.get(key))
            .or_else(|| default.and_then(|resources| resources.get(key)))
            .or_else(|| fallback.and_then(|resources| resources.get(key)))
            .cloned()
            .or_else(|| builtin_english_resources().get(key).cloned())
            .unwrap_or_else(|| key.to_string())
    }
}

fn preset_profile(preset: OutfitPreset) -> OutfitProfile {
    let (app_name, window_title, primary, zh_overrides, en_overrides) = match preset {
        OutfitPreset::ClewOriginal => ("Clew", "Clew", "#2684FF", vec![], vec![]),
        OutfitPreset::ResearchLab => (
            "Research Connect",
            "Research Connect",
            "#315B7D",
            vec![
                (KEY_READY, "这台电脑已准备好参与研究协作。"),
                (KEY_AWAITING_ENROLLMENT, "邀请已验证，正在接入研究项目。"),
            ],
            vec![
                (
                    KEY_READY,
                    "This computer is ready for research collaboration.",
                ),
                (
                    KEY_AWAITING_ENROLLMENT,
                    "Invitation verified. Joining the research project.",
                ),
            ],
        ),
        OutfitPreset::FriendlyMinimal => (
            "Connect",
            "Connect",
            "#3A7D5D",
            vec![
                (KEY_READY, "已经连好了，可以把窗口关掉。"),
                (KEY_AWAITING_ENROLLMENT, "正在连接，请把这个窗口开着。"),
            ],
            vec![
                (KEY_READY, "Connected. You can close this window."),
                (
                    KEY_AWAITING_ENROLLMENT,
                    "Connecting. Please keep this window open.",
                ),
            ],
        ),
        OutfitPreset::InstitutionClean => (
            "Collaboration Access",
            "Collaboration Access",
            "#3F4A56",
            vec![
                (KEY_READY, "协作连接已就绪。"),
                (KEY_AWAITING_ENROLLMENT, "邀请已验证，正在建立协作连接。"),
            ],
            vec![
                (KEY_READY, "Collaboration access is ready."),
                (
                    KEY_AWAITING_ENROLLMENT,
                    "Invitation verified. Establishing collaboration access.",
                ),
            ],
        ),
    };

    let mut zh = builtin_zh_cn_resources();
    for (key, value) in zh_overrides {
        zh.insert(key.into(), value.into());
    }
    let mut en = builtin_english_resources();
    for (key, value) in en_overrides {
        en.insert(key.into(), value.into());
    }
    let mut resources_by_locale = BTreeMap::new();
    resources_by_locale.insert("zh-CN".into(), zh);
    resources_by_locale.insert("en-US".into(), en);

    OutfitProfile {
        schema_version: OUTFIT_SCHEMA_VERSION,
        outfit_id: preset.id().into(),
        revision: 1,
        display_name: preset.display_name().into(),
        base_preset: preset,
        identity: OutfitIdentity {
            app_display_name: app_name.into(),
            window_title: window_title.into(),
            helper_window_title: None,
            publisher_label: None,
            artifact_name_template: "{site}-{app}-{platform}".into(),
        },
        visuals: OutfitVisuals {
            app_icon: OutfitAssetRef::BuiltIn {
                key: "clew-original".into(),
            },
            tray_icon_base: None,
            logo: None,
            key_visual: None,
            primary_color: primary.into(),
            secondary_color: None,
            surface_style: SurfaceStyle::Light,
        },
        strings: OutfitStrings {
            locale_default: "en-US".into(),
            locale_fallback: "en-US".into(),
            resources_by_locale,
        },
        distribution_copy: OutfitDistributionCopy {
            start_here_title: "Start here".into(),
            start_here_body:
                "Extract the complete archive first, then open the Clew app. Keep the app and site.clew together."
                    .into(),
            chat_message_template:
                "Extract the complete Site Kit first, then open the Clew app. Keep the app and site.clew together."
                    .into(),
            support_contact: None,
        },
    }
}

fn builtin_zh_cn_resources() -> BTreeMap<String, String> {
    BTreeMap::from([
        (KEY_READY.into(), "这台电脑已准备好。".into()),
        (
            KEY_AWAITING_ENROLLMENT.into(),
            "邀请已验证，正在等待连接 Controller。".into(),
        ),
        (KEY_MISSING_INVITE_TITLE.into(), "还缺一个邀请文件。".into()),
        (
            KEY_MISSING_INVITE_BODY.into(),
            "请把 site.clew 和这个程序放在同一个文件夹，或把 site.clew 拖到这里。".into(),
        ),
        (
            KEY_EXTRACT_FIRST.into(),
            "请先全部解压这个压缩包，再打开程序。".into(),
        ),
        (KEY_CHOOSE_INVITE.into(), "选择邀请文件".into()),
        (KEY_HIDE_TO_TRAY.into(), "隐藏到托盘".into()),
        (KEY_EXIT_AND_DISCONNECT.into(), "退出并断开".into()),
        (KEY_HELPER_READY.into(), "连接已就绪".into()),
        (KEY_TRAY_CONNECTED.into(), "已连接".into()),
        (KEY_TRAY_RECONNECTING.into(), "正在重连".into()),
        (KEY_TRAY_SHOW.into(), "显示".into()),
        (KEY_TRAY_EXIT.into(), "退出并断开".into()),
    ])
}

fn builtin_english_resources() -> BTreeMap<String, String> {
    BTreeMap::from([
        (KEY_READY.into(), "This computer is ready.".into()),
        (
            KEY_AWAITING_ENROLLMENT.into(),
            "Invitation verified. Connecting to the controller.".into(),
        ),
        (
            KEY_MISSING_INVITE_TITLE.into(),
            "An invitation file is still needed.".into(),
        ),
        (
            KEY_MISSING_INVITE_BODY.into(),
            "Keep site.clew next to this app, or drop site.clew here.".into(),
        ),
        (
            KEY_EXTRACT_FIRST.into(),
            "Extract the complete archive before opening the app.".into(),
        ),
        (KEY_CHOOSE_INVITE.into(), "Choose invitation file".into()),
        (KEY_HIDE_TO_TRAY.into(), "Hide to tray".into()),
        (KEY_EXIT_AND_DISCONNECT.into(), "Exit and disconnect".into()),
        (KEY_HELPER_READY.into(), "Connection helper is ready".into()),
        (KEY_TRAY_CONNECTED.into(), "Connected".into()),
        (KEY_TRAY_RECONNECTING.into(), "Reconnecting".into()),
        (KEY_TRAY_SHOW.into(), "Show".into()),
        (KEY_TRAY_EXIT.into(), "Exit and disconnect".into()),
    ])
}

fn validate_id(value: &str) -> Result<(), OutfitError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(OutfitError::InvalidId(value.into()));
    }
    Ok(())
}

fn validate_locale(value: &str) -> Result<(), OutfitError> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(OutfitError::InvalidLocale(value.into()));
    }
    Ok(())
}

fn validate_resource_key(value: &str) -> Result<(), OutfitError> {
    if value.is_empty()
        || value.len() > MAX_RESOURCE_KEY_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(OutfitError::InvalidResourceKey(value.into()));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), OutfitError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(OutfitError::InvalidText { field, max });
    }
    Ok(())
}

fn validate_color(value: &str) -> Result<(), OutfitError> {
    if value.len() != 7
        || !value.starts_with('#')
        || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(OutfitError::InvalidColor(value.into()));
    }
    Ok(())
}

impl OutfitProfile {
    #[must_use]
    pub fn imported_asset_ids(&self) -> Vec<String> {
        let mut ids = BTreeSet::new();
        for asset in [
            Some(&self.visuals.app_icon),
            self.visuals.tray_icon_base.as_ref(),
            self.visuals.logo.as_ref(),
            self.visuals.key_visual.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if let OutfitAssetRef::Imported { asset_id } = asset {
                ids.insert(asset_id.clone());
            }
        }
        ids.into_iter().collect()
    }
}

#[must_use]
pub fn outfit_asset_id_for_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::with_capacity(71);
    value.push_str("sha256-");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

pub fn verify_outfit_asset_bytes(asset_id: &str, bytes: &[u8]) -> Result<(), OutfitError> {
    if bytes.is_empty() || bytes.len() > MAX_OUTFIT_ASSET_BYTES {
        return Err(OutfitError::InvalidAssetBytes(bytes.len()));
    }
    if outfit_asset_id_for_bytes(bytes) != asset_id {
        return Err(OutfitError::AssetHashMismatch(asset_id.into()));
    }
    Ok(())
}

fn validate_asset(asset: &OutfitAssetRef) -> Result<(), OutfitError> {
    match asset {
        OutfitAssetRef::BuiltIn { key } => {
            if key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(OutfitError::InvalidAssetRef(key.clone()));
            }
        }
        OutfitAssetRef::Imported { asset_id } => {
            let Some(hex) = asset_id.strip_prefix("sha256-") else {
                return Err(OutfitError::InvalidAssetRef(asset_id.clone()));
            };
            if hex.len() != 64
                || !hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            {
                return Err(OutfitError::InvalidAssetRef(asset_id.clone()));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum OutfitError {
    #[error("unsupported outfit schema version {0}")]
    UnsupportedSchema(u32),
    #[error("outfit revision must be at least 1")]
    InvalidRevision,
    #[error("invalid outfit id {0:?}")]
    InvalidId(String),
    #[error(
        "invalid outfit text field {field}; it must be non-empty, bounded to {max} bytes, and contain no control characters"
    )]
    InvalidText { field: &'static str, max: usize },
    #[error("invalid outfit color {0:?}; expected #RRGGBB")]
    InvalidColor(String),
    #[error("invalid outfit locale {0:?}")]
    InvalidLocale(String),
    #[error("invalid outfit resource key {0:?}")]
    InvalidResourceKey(String),
    #[error("invalid outfit asset reference {0:?}")]
    InvalidAssetRef(String),
    #[error("outfit asset payload is empty or too large: {0} bytes")]
    InvalidAssetBytes(usize),
    #[error("outfit asset bytes do not match signed content id {0:?}")]
    AssetHashMismatch(String),
    #[error("outfit has too many locales: {0}")]
    TooManyLocales(usize),
    #[error("outfit locale {locale:?} has too many string resources: {count}")]
    TooManyResources { locale: String, count: usize },
    #[error("built-in English fallback is missing required key {0}")]
    MissingBuiltInFallback(String),
    #[error("unsupported Outfit build-spec schema version {0}")]
    UnsupportedBuildSpecSchema(u32),
    #[error("Outfit build-spec cache key does not match its profile")]
    BuildCacheKeyMismatch,
    #[error("Outfit build-spec asset set does not exactly match imported profile assets")]
    BuildAssetSetMismatch,
    #[error("invalid Outfit build-spec asset path {0:?}")]
    InvalidBuildAssetPath(String),
    #[error("invalid Outfit build-spec asset length for {asset_id}: {byte_len} bytes")]
    InvalidBuildAssetLength { asset_id: String, byte_len: u32 },
    #[error("Outfit build-spec JSON is too large: {0} bytes")]
    BuildSpecTooLarge(usize),
    #[error("encoded outfit profile is too large: {0} bytes")]
    EncodedTooLarge(usize),
    #[error("outfit JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_validate_and_roundtrip() {
        for preset in OutfitPreset::ALL {
            let profile = OutfitProfile::preset(preset);
            profile.validate().unwrap();
            let encoded = profile.encode().unwrap();
            assert_eq!(OutfitProfile::decode(&encoded).unwrap(), profile);
            assert!(profile.identity.app_display_name.is_ascii());
            assert!(profile.identity.window_title.is_ascii());
            assert!(profile.display_name.is_ascii());
        }
    }

    #[test]
    fn missing_locale_key_falls_back_to_builtin_english() {
        let mut profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        profile.strings.locale_default = "fr-CA".into();
        profile.strings.resources_by_locale.clear();
        profile
            .strings
            .resources_by_locale
            .insert("fr-CA".into(), BTreeMap::new());
        assert_eq!(
            profile.resolve_resource("fr-CA", KEY_EXIT_AND_DISCONNECT),
            "Exit and disconnect"
        );
    }

    #[test]
    fn build_cache_key_is_stable_and_changes_with_profile_content() {
        let profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        assert_eq!(
            profile.build_cache_key().unwrap(),
            profile.clone().build_cache_key().unwrap()
        );
        let mut changed = profile.clone();
        changed.visuals.primary_color = "#123456".into();
        assert_ne!(
            profile.build_cache_key().unwrap(),
            changed.build_cache_key().unwrap()
        );
        let key = profile.build_cache_key().unwrap();
        assert!(key.starts_with("outfit-v1-"));
        assert_eq!(key.len(), 74);
    }

    #[test]
    fn build_spec_binds_profile_cache_key_and_exact_imported_assets() {
        let mut profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        let bytes = b"svg-bytes";
        let asset_id = outfit_asset_id_for_bytes(bytes);
        profile.visuals.app_icon = OutfitAssetRef::Imported {
            asset_id: asset_id.clone(),
        };
        let spec = OutfitBuildSpec::new(
            profile.clone(),
            vec![OutfitBuildAsset {
                asset_id: asset_id.clone(),
                relative_path: format!("assets/{asset_id}.svg"),
                byte_len: bytes.len() as u32,
            }],
        )
        .unwrap();
        assert_eq!(
            OutfitBuildSpec::decode(&spec.encode().unwrap()).unwrap(),
            spec
        );

        let mut wrong_key = spec.clone();
        wrong_key.build_cache_key.push('0');
        assert!(matches!(
            wrong_key.validate(),
            Err(OutfitError::BuildCacheKeyMismatch)
        ));

        let mut missing = spec.clone();
        missing.assets.clear();
        assert!(matches!(
            missing.validate(),
            Err(OutfitError::BuildAssetSetMismatch)
        ));

        let mut unsafe_path = spec;
        unsafe_path.assets[0].relative_path = format!("assets/../{asset_id}.svg");
        assert!(matches!(
            unsafe_path.validate(),
            Err(OutfitError::InvalidBuildAssetPath(_)) | Err(OutfitError::BuildAssetSetMismatch)
        ));
    }

    #[test]
    fn profile_is_bounded_and_rejects_permission_like_freeform_expansion() {
        let mut profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        profile
            .strings
            .resources_by_locale
            .get_mut("zh-CN")
            .unwrap()
            .insert(
                "permission.shell".into(),
                "x".repeat(MAX_RESOURCE_VALUE_BYTES + 1),
            );
        assert!(matches!(
            profile.validate(),
            Err(OutfitError::InvalidText { .. })
        ));
    }

    #[test]
    fn colors_and_ids_fail_closed() {
        let mut profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        profile.outfit_id = "../../secret".into();
        assert!(matches!(profile.validate(), Err(OutfitError::InvalidId(_))));
        let mut profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        profile.visuals.primary_color = "transparent".into();
        assert!(matches!(
            profile.validate(),
            Err(OutfitError::InvalidColor(_))
        ));
    }
}
