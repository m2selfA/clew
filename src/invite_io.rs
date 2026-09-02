use std::io::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_host::{SignedSiteClew, verify_outfit_asset_bytes};
use clew_runtime::{LocalApiClient, OutfitAssetFormat};

pub async fn write_invitation(
    client: &LocalApiClient,
    site_file: &SignedSiteClew,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    write_outfit_assets(client, site_file, output).await?;
    site_file.write(output)?;
    Ok(())
}

async fn write_outfit_assets(
    client: &LocalApiClient,
    site_file: &SignedSiteClew,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(profile) = &site_file.payload.outfit_profile else {
        return Ok(());
    };
    let asset_ids = profile.imported_asset_ids();
    if asset_ids.is_empty() {
        return Ok(());
    }
    let kit_root = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let assets_root = kit_root.join("outfit-assets");
    std::fs::create_dir_all(&assets_root)?;
    for asset_id in asset_ids {
        let asset = client.outfit_asset_get(asset_id.clone()).await?;
        if asset.info.asset_id != asset_id {
            return Err("controller returned a different Outfit asset id".into());
        }
        let bytes = BASE64_STANDARD.decode(asset.data_base64.as_bytes())?;
        if usize::try_from(asset.info.byte_len).ok() != Some(bytes.len()) {
            return Err("controller returned inconsistent Outfit asset length".into());
        }
        verify_outfit_asset_bytes(&asset_id, &bytes)?;
        let extension = match asset.info.format {
            OutfitAssetFormat::Png => "png",
            OutfitAssetFormat::Svg => "svg",
        };
        write_asset_atomically(&assets_root, &asset_id, extension, &bytes)?;
    }
    Ok(())
}

fn write_asset_atomically(
    root: &std::path::Path,
    asset_id: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let target = root.join(format!("{asset_id}.{extension}"));
    if target.exists() {
        let existing = std::fs::read(&target)?;
        verify_outfit_asset_bytes(asset_id, &existing)?;
        return Ok(());
    }
    let temp = root.join(format!(
        ".{asset_id}.{extension}.{}.tmp",
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
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
    match std::fs::rename(&temp, &target) {
        Ok(()) => Ok(()),
        Err(error) if target.exists() => {
            let _ = std::fs::remove_file(&temp);
            let existing = std::fs::read(&target)?;
            verify_outfit_asset_bytes(asset_id, &existing)?;
            let _ = error;
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error.into())
        }
    }
}
