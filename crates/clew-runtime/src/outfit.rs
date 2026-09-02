use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use clew_core::{
    MAX_STATE_DOCUMENT_SIZE, StateCodecError, StateLayout, decode_state_json, encode_state_json,
};
use clew_host::{OutfitAssetRef, OutfitError, OutfitPreset, OutfitProfile};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_CUSTOM_OUTFITS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitLibraryEntry {
    pub outfit_id: String,
    pub display_name: String,
    pub revision: u32,
    pub base_preset: OutfitPreset,
    pub built_in: bool,
    pub is_default: bool,
    pub is_recent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitLibrarySnapshot {
    pub generation: u64,
    pub default_outfit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_outfit_id: Option<String>,
    pub custom: BTreeMap<String, OutfitProfile>,
}

impl OutfitLibrarySnapshot {
    fn initial() -> Self {
        Self {
            generation: 0,
            default_outfit_id: OutfitPreset::ClewOriginal.id().into(),
            recent_outfit_id: None,
            custom: BTreeMap::new(),
        }
    }

    fn validate(&self) -> Result<(), OutfitStoreError> {
        if self.custom.len() > MAX_CUSTOM_OUTFITS {
            return Err(OutfitStoreError::TooManyCustomOutfits(self.custom.len()));
        }
        for (id, profile) in &self.custom {
            if id != &profile.outfit_id {
                return Err(OutfitStoreError::ProfileKeyMismatch(id.clone()));
            }
            if preset_by_id(id).is_some() {
                return Err(OutfitStoreError::ReservedBuiltInId(id.clone()));
            }
            profile.validate()?;
        }
        if !self.contains_id(&self.default_outfit_id) {
            return Err(OutfitStoreError::UnknownOutfit(
                self.default_outfit_id.clone(),
            ));
        }
        if let Some(recent) = &self.recent_outfit_id
            && !self.contains_id(recent)
        {
            return Err(OutfitStoreError::UnknownOutfit(recent.clone()));
        }
        Ok(())
    }

    fn contains_id(&self, id: &str) -> bool {
        preset_by_id(id).is_some() || self.custom.contains_key(id)
    }
}

#[derive(Debug)]
pub struct OutfitLibrary {
    layout: StateLayout,
    snapshot: OutfitLibrarySnapshot,
}

impl OutfitLibrary {
    pub fn load_or_create(layout: StateLayout) -> Result<Self, OutfitStoreError> {
        let mut valid = Vec::new();
        let mut first_invalid = None;
        let mut any_present = false;
        for slot in [
            read_slot(&layout.outfit_library_slot_a_path()),
            read_slot(&layout.outfit_library_slot_b_path()),
        ] {
            match slot {
                SlotRead::Missing => {}
                SlotRead::Valid(snapshot) => {
                    any_present = true;
                    match snapshot.validate() {
                        Ok(()) => valid.push(snapshot),
                        Err(error) => {
                            first_invalid.get_or_insert(error);
                        }
                    }
                }
                SlotRead::Invalid(error) => {
                    any_present = true;
                    first_invalid.get_or_insert(error);
                }
            }
        }
        if !valid.is_empty() {
            valid.sort_by_key(|snapshot| snapshot.generation);
            if valid.len() == 2 && valid[0].generation == valid[1].generation {
                return Err(OutfitStoreError::GenerationConflict(valid[0].generation));
            }
            return Ok(Self {
                layout,
                snapshot: valid.pop().expect("valid outfit snapshot exists"),
            });
        }
        if any_present {
            return Err(first_invalid.unwrap_or(OutfitStoreError::NoValidSlot));
        }
        let mut store = Self {
            layout,
            snapshot: OutfitLibrarySnapshot::initial(),
        };
        store.commit_candidate(store.snapshot.clone())?;
        Ok(store)
    }

    #[must_use]
    pub fn snapshot(&self) -> &OutfitLibrarySnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn list(&self) -> Vec<OutfitLibraryEntry> {
        let mut entries = OutfitPreset::ALL
            .into_iter()
            .map(|preset| {
                let profile = preset.profile();
                self.entry_for(&profile, true)
            })
            .collect::<Vec<_>>();
        entries.extend(
            self.snapshot
                .custom
                .values()
                .map(|profile| self.entry_for(profile, false)),
        );
        entries
    }

    pub fn get(&self, outfit_id: &str) -> Result<OutfitProfile, OutfitStoreError> {
        if let Some(preset) = preset_by_id(outfit_id) {
            return Ok(preset.profile());
        }
        self.snapshot
            .custom
            .get(outfit_id)
            .cloned()
            .ok_or_else(|| OutfitStoreError::UnknownOutfit(outfit_id.into()))
    }

    pub fn create_from_preset(
        &mut self,
        outfit_id: String,
        display_name: String,
        preset: OutfitPreset,
    ) -> Result<OutfitProfile, OutfitStoreError> {
        if self.snapshot.contains_id(&outfit_id) {
            return Err(OutfitStoreError::OutfitAlreadyExists(outfit_id));
        }
        let mut profile = preset.profile();
        profile.outfit_id = outfit_id.clone();
        profile.display_name = display_name;
        profile.revision = 1;
        profile.validate()?;
        self.transaction(move |snapshot| {
            if snapshot.custom.len() >= MAX_CUSTOM_OUTFITS {
                return Err(OutfitStoreError::TooManyCustomOutfits(
                    snapshot.custom.len() + 1,
                ));
            }
            snapshot.custom.insert(outfit_id, profile.clone());
            snapshot.recent_outfit_id = Some(profile.outfit_id.clone());
            Ok(profile)
        })
    }

    pub fn clone_outfit(
        &mut self,
        source_id: &str,
        outfit_id: String,
        display_name: String,
    ) -> Result<OutfitProfile, OutfitStoreError> {
        if self.snapshot.contains_id(&outfit_id) {
            return Err(OutfitStoreError::OutfitAlreadyExists(outfit_id));
        }
        let mut profile = self.get(source_id)?;
        profile.outfit_id = outfit_id.clone();
        profile.display_name = display_name;
        profile.revision = 1;
        profile.validate()?;
        self.transaction(move |snapshot| {
            if snapshot.custom.len() >= MAX_CUSTOM_OUTFITS {
                return Err(OutfitStoreError::TooManyCustomOutfits(
                    snapshot.custom.len() + 1,
                ));
            }
            snapshot.custom.insert(outfit_id, profile.clone());
            snapshot.recent_outfit_id = Some(profile.outfit_id.clone());
            Ok(profile)
        })
    }

    pub fn set_default(&mut self, outfit_id: &str) -> Result<(), OutfitStoreError> {
        if !self.snapshot.contains_id(outfit_id) {
            return Err(OutfitStoreError::UnknownOutfit(outfit_id.into()));
        }
        let outfit_id = outfit_id.to_owned();
        self.transaction(move |snapshot| {
            snapshot.default_outfit_id = outfit_id;
            Ok(())
        })
    }

    pub fn mark_recent(&mut self, outfit_id: &str) -> Result<(), OutfitStoreError> {
        if !self.snapshot.contains_id(outfit_id) {
            return Err(OutfitStoreError::UnknownOutfit(outfit_id.into()));
        }
        let outfit_id = outfit_id.to_owned();
        self.transaction(move |snapshot| {
            snapshot.recent_outfit_id = Some(outfit_id);
            Ok(())
        })
    }

    pub fn set_field(
        &mut self,
        outfit_id: &str,
        field: &str,
        value: String,
    ) -> Result<OutfitProfile, OutfitStoreError> {
        if preset_by_id(outfit_id).is_some() {
            return Err(OutfitStoreError::BuiltInIsReadOnly(outfit_id.into()));
        }
        let outfit_id = outfit_id.to_owned();
        let field = field.to_owned();
        self.transaction(move |snapshot| {
            let profile = snapshot
                .custom
                .get_mut(&outfit_id)
                .ok_or_else(|| OutfitStoreError::UnknownOutfit(outfit_id.clone()))?;
            let changed = match field.as_str() {
                "display-name" => replace_if_changed(&mut profile.display_name, value),
                "identity.app-display-name" => {
                    replace_if_changed(&mut profile.identity.app_display_name, value)
                }
                "identity.window-title" => {
                    replace_if_changed(&mut profile.identity.window_title, value)
                }
                "visuals.primary-color" => {
                    replace_if_changed(&mut profile.visuals.primary_color, value)
                }
                "distribution.start-here-title" => {
                    replace_if_changed(&mut profile.distribution_copy.start_here_title, value)
                }
                "distribution.start-here-body" => {
                    replace_if_changed(&mut profile.distribution_copy.start_here_body, value)
                }
                "distribution.chat-message-template" => {
                    replace_if_changed(&mut profile.distribution_copy.chat_message_template, value)
                }
                _ => return Err(OutfitStoreError::UnsupportedField(field.clone())),
            };
            if changed {
                profile.revision = profile
                    .revision
                    .checked_add(1)
                    .ok_or(OutfitStoreError::RevisionOverflow)?;
            }
            profile.validate()?;
            snapshot.recent_outfit_id = Some(profile.outfit_id.clone());
            Ok(profile.clone())
        })
    }

    pub fn set_asset(
        &mut self,
        outfit_id: &str,
        slot: &str,
        asset: OutfitAssetRef,
    ) -> Result<OutfitProfile, OutfitStoreError> {
        if preset_by_id(outfit_id).is_some() {
            return Err(OutfitStoreError::BuiltInIsReadOnly(outfit_id.into()));
        }
        let outfit_id = outfit_id.to_owned();
        let slot = slot.to_owned();
        self.transaction(move |snapshot| {
            let profile = snapshot
                .custom
                .get_mut(&outfit_id)
                .ok_or_else(|| OutfitStoreError::UnknownOutfit(outfit_id.clone()))?;
            let changed = match slot.as_str() {
                "app-icon" => replace_asset_if_changed(&mut profile.visuals.app_icon, asset),
                "tray-icon" => replace_optional_asset_if_changed(
                    &mut profile.visuals.tray_icon_base,
                    Some(asset),
                ),
                "logo" => replace_optional_asset_if_changed(&mut profile.visuals.logo, Some(asset)),
                "key-visual" => {
                    replace_optional_asset_if_changed(&mut profile.visuals.key_visual, Some(asset))
                }
                _ => return Err(OutfitStoreError::UnsupportedAssetSlot(slot.clone())),
            };
            if changed {
                profile.revision = profile
                    .revision
                    .checked_add(1)
                    .ok_or(OutfitStoreError::RevisionOverflow)?;
            }
            profile.validate()?;
            snapshot.recent_outfit_id = Some(profile.outfit_id.clone());
            Ok(profile.clone())
        })
    }

    fn entry_for(&self, profile: &OutfitProfile, built_in: bool) -> OutfitLibraryEntry {
        OutfitLibraryEntry {
            outfit_id: profile.outfit_id.clone(),
            display_name: profile.display_name.clone(),
            revision: profile.revision,
            base_preset: profile.base_preset,
            built_in,
            is_default: self.snapshot.default_outfit_id == profile.outfit_id,
            is_recent: self.snapshot.recent_outfit_id.as_deref() == Some(&profile.outfit_id),
        }
    }

    fn transaction<R>(
        &mut self,
        mutate: impl FnOnce(&mut OutfitLibrarySnapshot) -> Result<R, OutfitStoreError>,
    ) -> Result<R, OutfitStoreError> {
        let mut candidate = self.snapshot.clone();
        let result = mutate(&mut candidate)?;
        candidate.validate()?;
        self.commit_candidate(candidate)?;
        Ok(result)
    }

    fn commit_candidate(
        &mut self,
        mut candidate: OutfitLibrarySnapshot,
    ) -> Result<(), OutfitStoreError> {
        candidate.generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or(OutfitStoreError::GenerationOverflow)?;
        candidate.validate()?;
        let path = if candidate.generation % 2 == 1 {
            self.layout.outfit_library_slot_a_path()
        } else {
            self.layout.outfit_library_slot_b_path()
        };
        write_slot(&path, &candidate)?;
        self.snapshot = candidate;
        Ok(())
    }
}

fn preset_by_id(id: &str) -> Option<OutfitPreset> {
    OutfitPreset::ALL
        .into_iter()
        .find(|preset| preset.id() == id)
}

fn replace_if_changed(target: &mut String, value: String) -> bool {
    if target == &value {
        return false;
    }
    *target = value;
    true
}

fn replace_asset_if_changed(target: &mut OutfitAssetRef, value: OutfitAssetRef) -> bool {
    if target == &value {
        return false;
    }
    *target = value;
    true
}

fn replace_optional_asset_if_changed(
    target: &mut Option<OutfitAssetRef>,
    value: Option<OutfitAssetRef>,
) -> bool {
    if target == &value {
        return false;
    }
    *target = value;
    true
}

enum SlotRead {
    Missing,
    Valid(OutfitLibrarySnapshot),
    Invalid(OutfitStoreError),
}

fn read_slot(path: &Path) -> SlotRead {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SlotRead::Missing,
        Err(error) => return SlotRead::Invalid(error.into()),
    };
    if metadata.len() > MAX_STATE_DOCUMENT_SIZE as u64 {
        return SlotRead::Invalid(OutfitStoreError::DocumentTooLarge(metadata.len()));
    }
    match fs::read(path)
        .map_err(OutfitStoreError::from)
        .and_then(|bytes| decode_state_json(&bytes).map_err(OutfitStoreError::from))
    {
        Ok(snapshot) => SlotRead::Valid(snapshot),
        Err(error) => SlotRead::Invalid(error),
    }
}

fn write_slot(path: &Path, snapshot: &OutfitLibrarySnapshot) -> Result<(), OutfitStoreError> {
    let parent = path.parent().ok_or(OutfitStoreError::InvalidStatePath)?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let encoded = encode_state_json(snapshot)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&encoded)?;
    file.sync_all()?;
    sync_parent(parent)?;
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
pub enum OutfitStoreError {
    #[error("outfit library I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] StateCodecError),
    #[error(transparent)]
    Outfit(#[from] OutfitError),
    #[error("outfit library state has no valid slot")]
    NoValidSlot,
    #[error("outfit library slots have conflicting generation {0}")]
    GenerationConflict(u64),
    #[error("outfit library generation overflow")]
    GenerationOverflow,
    #[error("outfit revision overflow")]
    RevisionOverflow,
    #[error("outfit library document is too large: {0} bytes")]
    DocumentTooLarge(u64),
    #[error("outfit library state path has no parent")]
    InvalidStatePath,
    #[error("outfit library contains too many custom profiles: {0}")]
    TooManyCustomOutfits(usize),
    #[error("outfit profile key does not match profile id {0:?}")]
    ProfileKeyMismatch(String),
    #[error("outfit id {0:?} is reserved for a built-in preset")]
    ReservedBuiltInId(String),
    #[error("outfit {0:?} already exists")]
    OutfitAlreadyExists(String),
    #[error("unknown outfit {0:?}")]
    UnknownOutfit(String),
    #[error("built-in outfit {0:?} is read-only; clone it before editing")]
    BuiltInIsReadOnly(String),
    #[error("unsupported outfit asset slot {0:?}")]
    UnsupportedAssetSlot(String),
    #[error("unsupported outfit field {0:?}")]
    UnsupportedField(String),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn builtins_are_synthesized_and_custom_revision_persists() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let mut store = OutfitLibrary::load_or_create(layout.clone()).unwrap();
        assert_eq!(store.list().len(), 4);
        let created = store
            .create_from_preset(
                "huang-lab".into(),
                "Huang Lab".into(),
                OutfitPreset::ResearchLab,
            )
            .unwrap();
        assert_eq!(created.revision, 1);
        let updated = store
            .set_field("huang-lab", "visuals.primary-color", "#2A6FBB".into())
            .unwrap();
        assert_eq!(updated.revision, 2);
        drop(store);
        let reloaded = OutfitLibrary::load_or_create(layout).unwrap();
        assert_eq!(reloaded.get("huang-lab").unwrap().revision, 2);
        assert_eq!(
            reloaded.snapshot().recent_outfit_id.as_deref(),
            Some("huang-lab")
        );
    }

    #[test]
    fn builtins_are_read_only_but_clone_is_editable() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let mut store = OutfitLibrary::load_or_create(layout).unwrap();
        assert!(matches!(
            store.set_field("clew-original", "display-name", "No".into()),
            Err(OutfitStoreError::BuiltInIsReadOnly(_))
        ));
        store
            .clone_outfit("clew-original", "project-x".into(), "Project X".into())
            .unwrap();
        let edited = store
            .set_field(
                "project-x",
                "identity.app-display-name",
                "Project X Connect".into(),
            )
            .unwrap();
        assert_eq!(edited.revision, 2);
    }

    #[test]
    fn imported_asset_reference_updates_custom_revision_only_when_changed() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let mut store = OutfitLibrary::load_or_create(layout).unwrap();
        store
            .create_from_preset("lab".into(), "Lab".into(), OutfitPreset::ResearchLab)
            .unwrap();
        let asset = OutfitAssetRef::Imported {
            asset_id: format!("sha256-{}", "a".repeat(64)),
        };
        let updated = store.set_asset("lab", "app-icon", asset.clone()).unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.visuals.app_icon, asset);
        let unchanged = store
            .set_asset("lab", "app-icon", updated.visuals.app_icon.clone())
            .unwrap();
        assert_eq!(unchanged.revision, 2);
        assert!(matches!(
            store.set_asset(
                "clew-original",
                "app-icon",
                OutfitAssetRef::BuiltIn { key: "x".into() }
            ),
            Err(OutfitStoreError::BuiltInIsReadOnly(_))
        ));
    }

    #[test]
    fn corrupt_newest_slot_recovers_previous_generation() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let mut store = OutfitLibrary::load_or_create(layout.clone()).unwrap();
        store
            .create_from_preset("lab".into(), "Lab".into(), OutfitPreset::ResearchLab)
            .unwrap();
        let newest = if store.snapshot().generation % 2 == 1 {
            layout.outfit_library_slot_a_path()
        } else {
            layout.outfit_library_slot_b_path()
        };
        fs::write(newest, b"broken").unwrap();
        let recovered = OutfitLibrary::load_or_create(layout).unwrap();
        assert!(recovered.snapshot().generation < store.snapshot().generation);
    }

    #[test]
    fn existing_but_unreadable_library_never_silently_resets() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        fs::create_dir_all(layout.version_root()).unwrap();
        fs::write(layout.outfit_library_slot_a_path(), b"broken").unwrap();
        assert!(OutfitLibrary::load_or_create(layout).is_err());
    }
}
