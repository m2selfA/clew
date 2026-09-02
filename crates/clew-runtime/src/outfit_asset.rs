use std::{
    fs::{self, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

use clew_core::StateLayout;
use clew_host::{outfit_asset_id_for_bytes, verify_outfit_asset_bytes};
use image::{GenericImageView, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub use clew_host::MAX_OUTFIT_ASSET_BYTES;
pub const MAX_OUTFIT_ASSETS: usize = 128;
pub const MAX_OUTFIT_ASSET_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_OUTFIT_RASTER_DIMENSION: u32 = 2048;
pub const MAX_OUTFIT_SVG_DIMENSION: f32 = 4096.0;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ASSET_ID_PREFIX: &str = "sha256-";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutfitAssetFormat {
    Png,
    Svg,
}

impl OutfitAssetFormat {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitAssetInfo {
    pub asset_id: String,
    pub format: OutfitAssetFormat,
    pub byte_len: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutfitAssetData {
    pub info: OutfitAssetInfo,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct OutfitAssetStore {
    layout: StateLayout,
}

impl OutfitAssetStore {
    pub fn load_or_create(layout: StateLayout) -> Result<Self, OutfitAssetError> {
        let root = layout.outfit_assets_root();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let store = Self { layout };
        store.validate_store_bounds()?;
        Ok(store)
    }

    pub fn import_path(&self, source: &Path) -> Result<OutfitAssetInfo, OutfitAssetError> {
        let metadata = fs::metadata(source)?;
        if !metadata.is_file() {
            return Err(OutfitAssetError::NotRegularFile);
        }
        if metadata.len() == 0 || metadata.len() > MAX_OUTFIT_ASSET_BYTES as u64 {
            return Err(OutfitAssetError::AssetTooLarge(metadata.len()));
        }
        let mut file = fs::File::open(source)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take((MAX_OUTFIT_ASSET_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_OUTFIT_ASSET_BYTES {
            return Err(OutfitAssetError::AssetTooLarge(bytes.len() as u64));
        }
        self.import_bytes(&bytes)
    }

    pub fn import_bytes(&self, bytes: &[u8]) -> Result<OutfitAssetInfo, OutfitAssetError> {
        if bytes.is_empty() || bytes.len() > MAX_OUTFIT_ASSET_BYTES {
            return Err(OutfitAssetError::AssetTooLarge(bytes.len() as u64));
        }
        let (format, width, height) = validate_asset_bytes(bytes)?;
        let asset_id = outfit_asset_id_for_bytes(bytes);
        let target = self.asset_path(&asset_id, format)?;
        if target.exists() {
            let existing = self.read(&asset_id)?;
            if existing.bytes != bytes || existing.info.format != format {
                return Err(OutfitAssetError::HashCollision(asset_id));
            }
            return Ok(existing.info);
        }

        let (count, total) = self.store_usage()?;
        if count >= MAX_OUTFIT_ASSETS {
            return Err(OutfitAssetError::TooManyAssets(count + 1));
        }
        let next_total = total.saturating_add(bytes.len() as u64);
        if next_total > MAX_OUTFIT_ASSET_TOTAL_BYTES {
            return Err(OutfitAssetError::StoreTooLarge(next_total));
        }

        let root = self.layout.outfit_assets_root();
        let temp = root.join(format!(".asset-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        }
        sync_parent(&root)?;
        Ok(OutfitAssetInfo {
            asset_id,
            format,
            byte_len: bytes.len() as u32,
            width,
            height,
        })
    }

    pub fn list(&self) -> Result<Vec<OutfitAssetInfo>, OutfitAssetError> {
        let root = self.layout.outfit_assets_root();
        let mut assets = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let Some((asset_id, _format)) = asset_identity_from_path(&path) else {
                continue;
            };
            assets.push(self.read(&asset_id)?.info);
            if assets.len() > MAX_OUTFIT_ASSETS {
                return Err(OutfitAssetError::TooManyAssets(assets.len()));
            }
        }
        assets.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
        Ok(assets)
    }

    pub fn read(&self, asset_id: &str) -> Result<OutfitAssetData, OutfitAssetError> {
        validate_asset_id(asset_id)?;
        let mut found = None;
        for format in [OutfitAssetFormat::Png, OutfitAssetFormat::Svg] {
            let path = self.asset_path(asset_id, format)?;
            if path.exists() {
                if found.is_some() {
                    return Err(OutfitAssetError::AmbiguousAsset(asset_id.into()));
                }
                found = Some((format, path));
            }
        }
        let (format, path) =
            found.ok_or_else(|| OutfitAssetError::UnknownAsset(asset_id.into()))?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() > MAX_OUTFIT_ASSET_BYTES as u64 {
            return Err(OutfitAssetError::CorruptAsset(asset_id.into()));
        }
        let bytes = fs::read(&path)?;
        if verify_outfit_asset_bytes(asset_id, &bytes).is_err() {
            return Err(OutfitAssetError::CorruptAsset(asset_id.into()));
        }
        let (validated_format, width, height) = validate_asset_bytes(&bytes)?;
        if validated_format != format {
            return Err(OutfitAssetError::CorruptAsset(asset_id.into()));
        }
        Ok(OutfitAssetData {
            info: OutfitAssetInfo {
                asset_id: asset_id.into(),
                format,
                byte_len: bytes.len() as u32,
                width,
                height,
            },
            bytes,
        })
    }

    fn asset_path(
        &self,
        asset_id: &str,
        format: OutfitAssetFormat,
    ) -> Result<PathBuf, OutfitAssetError> {
        validate_asset_id(asset_id)?;
        Ok(self
            .layout
            .outfit_assets_root()
            .join(format!("{asset_id}.{}", format.extension())))
    }

    fn store_usage(&self) -> Result<(usize, u64), OutfitAssetError> {
        let root = self.layout.outfit_assets_root();
        let mut count = 0_usize;
        let mut total = 0_u64;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || asset_identity_from_path(&entry.path()).is_none() {
                continue;
            }
            count += 1;
            total = total.saturating_add(entry.metadata()?.len());
            if count > MAX_OUTFIT_ASSETS {
                return Err(OutfitAssetError::TooManyAssets(count));
            }
            if total > MAX_OUTFIT_ASSET_TOTAL_BYTES {
                return Err(OutfitAssetError::StoreTooLarge(total));
            }
        }
        Ok((count, total))
    }

    fn validate_store_bounds(&self) -> Result<(), OutfitAssetError> {
        let _ = self.store_usage()?;
        Ok(())
    }
}

fn validate_asset_id(value: &str) -> Result<(), OutfitAssetError> {
    let Some(hex) = value.strip_prefix(ASSET_ID_PREFIX) else {
        return Err(OutfitAssetError::InvalidAssetId(value.into()));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(OutfitAssetError::InvalidAssetId(value.into()));
    }
    Ok(())
}

fn asset_identity_from_path(path: &Path) -> Option<(String, OutfitAssetFormat)> {
    let extension = path.extension()?.to_str()?;
    let format = match extension {
        "png" => OutfitAssetFormat::Png,
        "svg" => OutfitAssetFormat::Svg,
        _ => return None,
    };
    let asset_id = path.file_stem()?.to_str()?.to_owned();
    validate_asset_id(&asset_id).ok()?;
    Some((asset_id, format))
}

fn validate_asset_bytes(bytes: &[u8]) -> Result<(OutfitAssetFormat, u32, u32), OutfitAssetError> {
    if bytes.starts_with(PNG_SIGNATURE) {
        let reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
        let (width, height) = reader
            .into_dimensions()
            .map_err(|error| OutfitAssetError::InvalidPng(error.to_string()))?;
        if width == 0
            || height == 0
            || width > MAX_OUTFIT_RASTER_DIMENSION
            || height > MAX_OUTFIT_RASTER_DIMENSION
        {
            return Err(OutfitAssetError::InvalidDimensions { width, height });
        }
        let image = image::load_from_memory_with_format(bytes, ImageFormat::Png)
            .map_err(|error| OutfitAssetError::InvalidPng(error.to_string()))?;
        if image.dimensions() != (width, height) {
            return Err(OutfitAssetError::InvalidPng("dimension mismatch".into()));
        }
        return Ok((OutfitAssetFormat::Png, width, height));
    }

    let text = std::str::from_utf8(bytes).map_err(|_| OutfitAssetError::UnsupportedFormat)?;
    validate_svg_text(text)?;
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &options)
        .map_err(|error| OutfitAssetError::InvalidSvg(error.to_string()))?;
    let size = tree.size();
    let width = size.width();
    let height = size.height();
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > MAX_OUTFIT_SVG_DIMENSION
        || height > MAX_OUTFIT_SVG_DIMENSION
    {
        return Err(OutfitAssetError::InvalidSvgDimensions { width, height });
    }
    Ok((
        OutfitAssetFormat::Svg,
        width.ceil() as u32,
        height.ceil() as u32,
    ))
}

fn validate_svg_text(text: &str) -> Result<(), OutfitAssetError> {
    let lower = text.to_ascii_lowercase();
    for forbidden in [
        "<!doctype",
        "<!entity",
        "<script",
        "<foreignobject",
        "<style",
        "@import",
    ] {
        if lower.contains(forbidden) {
            return Err(OutfitAssetError::UnsafeSvgReference);
        }
    }
    let mut css_rest = lower.as_str();
    while let Some(index) = css_rest.find("url(") {
        css_rest = &css_rest[index + 4..];
        let Some(end) = css_rest.find(')') else {
            return Err(OutfitAssetError::UnsafeSvgReference);
        };
        let target = css_rest[..end]
            .trim()
            .trim_matches(|value| matches!(value, '\'' | '"'));
        if !(target.starts_with('#') || target.starts_with("data:")) {
            return Err(OutfitAssetError::UnsafeSvgReference);
        }
        css_rest = &css_rest[end + 1..];
    }

    let mut rest = lower.as_str();
    while let Some(index) = rest.find("href=") {
        rest = &rest[index + 5..];
        let trimmed = rest.trim_start();
        let Some(quote) = trimmed.chars().next() else {
            break;
        };
        if !matches!(quote, '\'' | '"') {
            return Err(OutfitAssetError::UnsafeSvgReference);
        }
        let value = &trimmed[1..];
        let Some(end) = value.find(quote) else {
            return Err(OutfitAssetError::UnsafeSvgReference);
        };
        let target = value[..end].trim();
        if !(target.starts_with('#') || target.starts_with("data:")) {
            return Err(OutfitAssetError::UnsafeSvgReference);
        }
        rest = &value[end + 1..];
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), std::io::Error> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum OutfitAssetError {
    #[error("outfit asset I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("outfit asset must be a regular file")]
    NotRegularFile,
    #[error("outfit asset is empty or too large: {0} bytes")]
    AssetTooLarge(u64),
    #[error("outfit asset format must be PNG or SVG")]
    UnsupportedFormat,
    #[error("invalid PNG outfit asset: {0}")]
    InvalidPng(String),
    #[error("invalid SVG outfit asset: {0}")]
    InvalidSvg(String),
    #[error("SVG outfit asset contains an unsafe external/script reference")]
    UnsafeSvgReference,
    #[error("outfit raster asset dimensions are invalid: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("outfit SVG dimensions are invalid: {width}x{height}")]
    InvalidSvgDimensions { width: f32, height: f32 },
    #[error("outfit asset id is invalid: {0:?}")]
    InvalidAssetId(String),
    #[error("unknown outfit asset {0:?}")]
    UnknownAsset(String),
    #[error("outfit asset has conflicting stored formats: {0:?}")]
    AmbiguousAsset(String),
    #[error("stored outfit asset is corrupt: {0:?}")]
    CorruptAsset(String),
    #[error("outfit asset hash collision detected: {0:?}")]
    HashCollision(String),
    #[error("outfit asset store contains too many assets: {0}")]
    TooManyAssets(usize),
    #[error("outfit asset store exceeds its byte budget: {0}")]
    StoreTooLarge(u64),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn png_fixture() -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(16, 12, image::Rgba([20, 40, 60, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn png_import_is_content_addressed_and_roundtrips() {
        let temp = tempdir().unwrap();
        let store = OutfitAssetStore::load_or_create(StateLayout::new(temp.path())).unwrap();
        let bytes = png_fixture();
        let first = store.import_bytes(&bytes).unwrap();
        let second = store.import_bytes(&bytes).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.format, OutfitAssetFormat::Png);
        assert_eq!((first.width, first.height), (16, 12));
        assert_eq!(store.read(&first.asset_id).unwrap().bytes, bytes);
        assert_eq!(store.list().unwrap(), vec![first]);
    }

    #[test]
    fn safe_svg_imports_but_external_refs_and_scripts_fail_closed() {
        let temp = tempdir().unwrap();
        let store = OutfitAssetStore::load_or_create(StateLayout::new(temp.path())).unwrap();
        let safe = br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="24"><rect width="32" height="24" fill="#2684ff"/></svg>"##;
        let info = store.import_bytes(safe).unwrap();
        assert_eq!(info.format, OutfitAssetFormat::Svg);
        assert_eq!((info.width, info.height), (32, 24));
        let external = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="24"><image href="file:///etc/passwd"/></svg>"#;
        assert!(matches!(
            store.import_bytes(external),
            Err(OutfitAssetError::UnsafeSvgReference)
        ));
        let script = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="24"><script>alert(1)</script></svg>"#;
        assert!(matches!(
            store.import_bytes(script),
            Err(OutfitAssetError::UnsafeSvgReference)
        ));
        let css_external = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="24"><rect style="fill:url(https://example.invalid/x.svg)" width="32" height="24"/></svg>"#;
        assert!(matches!(
            store.import_bytes(css_external),
            Err(OutfitAssetError::UnsafeSvgReference)
        ));
        let local_gradient = br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="24"><defs><linearGradient id="g"><stop stop-color="#fff"/></linearGradient></defs><rect fill="url(#g)" width="32" height="24"/></svg>"##;
        assert!(store.import_bytes(local_gradient).is_ok());
    }

    #[test]
    fn oversized_and_invalid_assets_fail_before_store_growth() {
        let temp = tempdir().unwrap();
        let store = OutfitAssetStore::load_or_create(StateLayout::new(temp.path())).unwrap();
        assert!(matches!(
            store.import_bytes(&vec![0_u8; MAX_OUTFIT_ASSET_BYTES + 1]),
            Err(OutfitAssetError::AssetTooLarge(_))
        ));
        assert!(matches!(
            store.import_bytes(b"not an image"),
            Err(OutfitAssetError::InvalidSvg(_)) | Err(OutfitAssetError::UnsupportedFormat)
        ));
        assert!(store.list().unwrap().is_empty());
    }
}
