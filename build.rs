use std::{env, error::Error, fs, io::Cursor, path::PathBuf};

use image::{DynamicImage, ImageFormat, RgbaImage};

const APP_ICON_SVG: &str = "assets/icons/app.svg";
const BUILD_APP_NAME_ENV: &str = "CLEW_BUILD_APP_NAME";
const BUILD_ICON_PATH_ENV: &str = "CLEW_BUILD_ICON_PATH";
const BUILD_OUTFIT_KEY_ENV: &str = "CLEW_BUILD_OUTFIT_KEY";
const BUILD_PUBLISHER_ENV: &str = "CLEW_BUILD_PUBLISHER";
const PRODUCT_NAME: &str = "Clew";

fn main() {
    println!("cargo:rerun-if-changed={APP_ICON_SVG}");
    for key in [
        BUILD_APP_NAME_ENV,
        BUILD_ICON_PATH_ENV,
        BUILD_OUTFIT_KEY_ENV,
        BUILD_PUBLISHER_ENV,
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    match env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => println!("cargo:rustc-link-arg=/STACK:8388608"),
        Ok("gnu") => println!("cargo:rustc-link-arg=-Wl,--stack,8388608"),
        _ => {}
    }
    if let Err(error) = compile_windows_resources() {
        panic!("failed to compile Clew Windows resources: {error}");
    }
}

fn compile_windows_resources() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let source_icon_path = env::var_os(BUILD_ICON_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(APP_ICON_SVG));
    println!("cargo:rerun-if-changed={}", source_icon_path.display());
    let icon_bytes = fs::read(&source_icon_path)?;
    let icon_path = out_dir.join("clew-app.ico");
    fs::write(&icon_path, build_ico_from_source(&icon_bytes)?)?;

    let version = env::var("CARGO_PKG_VERSION")?;
    let product_name = env::var(BUILD_APP_NAME_ENV).unwrap_or_else(|_| PRODUCT_NAME.into());
    validate_resource_text(&product_name, "application display name")?;
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&icon_path.to_string_lossy());
    resource.set("ProductName", &product_name);
    resource.set("FileDescription", &product_name);
    if let Ok(publisher) = env::var(BUILD_PUBLISHER_ENV) {
        validate_resource_text(&publisher, "publisher label")?;
        resource.set("CompanyName", &publisher);
    }
    resource.set("InternalName", "clew");
    resource.set("OriginalFilename", "clew.exe");
    resource.set("FileVersion", &version);
    resource.set("ProductVersion", &version);
    resource.compile()?;
    Ok(())
}

fn validate_resource_text(value: &str, label: &str) -> Result<(), Box<dyn Error>> {
    if value.trim().is_empty() || value.len() > 96 || value.chars().any(char::is_control) {
        return Err(format!("invalid {label} for Windows resources").into());
    }
    Ok(())
}

fn build_ico_from_source(source: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let sizes = [16_u32, 24, 32, 48, 64, 128, 256];
    let images = sizes
        .into_iter()
        .map(|size| render_icon_png(source, size).map(|png| (size, png)))
        .collect::<Result<Vec<_>, _>>()?;

    let count = u16::try_from(images.len())?;
    let directory_bytes = 6_usize + images.len() * 16;
    let mut offset = u32::try_from(directory_bytes)?;
    let mut ico = Vec::with_capacity(
        directory_bytes + images.iter().map(|(_, png)| png.len()).sum::<usize>(),
    );
    ico.extend_from_slice(&0_u16.to_le_bytes());
    ico.extend_from_slice(&1_u16.to_le_bytes());
    ico.extend_from_slice(&count.to_le_bytes());
    for (size, png) in &images {
        ico.push(if *size >= 256 { 0 } else { *size as u8 });
        ico.push(if *size >= 256 { 0 } else { *size as u8 });
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1_u16.to_le_bytes());
        ico.extend_from_slice(&32_u16.to_le_bytes());
        ico.extend_from_slice(&u32::try_from(png.len())?.to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset = offset
            .checked_add(u32::try_from(png.len())?)
            .ok_or("ICO size overflow")?;
    }
    for (_, png) in images {
        ico.extend_from_slice(&png);
    }
    Ok(ico)
}

fn render_icon_png(source: &[u8], size: u32) -> Result<Vec<u8>, Box<dyn Error>> {
    let image = if source.starts_with(b"\x89PNG\r\n\x1a\n") {
        image::load_from_memory_with_format(source, ImageFormat::Png)?
            .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
    } else {
        let options = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_data(source, &options)?;
        let source_size = tree.size();
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(size, size).ok_or("icon pixmap allocation failed")?;
        let transform = resvg::tiny_skia::Transform::from_scale(
            size as f32 / source_size.width(),
            size as f32 / source_size.height(),
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let mut rgba = pixmap.take();
        unpremultiply_rgba(&mut rgba);
        RgbaImage::from_raw(size, size, rgba).ok_or("invalid rendered RGBA icon")?
    };
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut cursor, ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let value = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = value.min(255) as u8;
        }
    }
}
