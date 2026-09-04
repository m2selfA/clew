use std::{env, error::Error, fs, io::Cursor, path::PathBuf};

use image::{DynamicImage, ImageFormat, RgbaImage};

const APP_ICON_SVG: &str = "assets/icons/app.svg";
const PRODUCT_NAME: &str = "Clew";
const FILE_DESCRIPTION: &str = "Agent-facing remote capability bridge";

fn main() {
    println!("cargo:rerun-if-changed={APP_ICON_SVG}");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if let Err(error) = compile_windows_resources() {
        panic!("failed to compile Clew Windows resources: {error}");
    }
}

fn compile_windows_resources() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let svg = fs::read(APP_ICON_SVG)?;
    let icon_path = out_dir.join("clew-app.ico");
    fs::write(&icon_path, build_ico_from_svg(&svg)?)?;

    let version = env::var("CARGO_PKG_VERSION")?;
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&icon_path.to_string_lossy());
    resource.set("ProductName", PRODUCT_NAME);
    resource.set("FileDescription", FILE_DESCRIPTION);
    resource.set("InternalName", "clew");
    resource.set("OriginalFilename", "clew.exe");
    resource.set("FileVersion", &version);
    resource.set("ProductVersion", &version);
    resource.compile()?;
    Ok(())
}

fn build_ico_from_svg(svg: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let sizes = [16_u32, 24, 32, 48, 64, 128, 256];
    let images = sizes
        .into_iter()
        .map(|size| render_svg_png(svg, size).map(|png| (size, png)))
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

fn render_svg_png(svg: &[u8], size: u32) -> Result<Vec<u8>, Box<dyn Error>> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg, &options)?;
    let source = tree.size();
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size, size).ok_or("icon pixmap allocation failed")?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        size as f32 / source.width(),
        size as f32 / source.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    let image = RgbaImage::from_raw(size, size, rgba).ok_or("invalid rendered RGBA icon")?;
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
