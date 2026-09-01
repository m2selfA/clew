use std::collections::{BTreeMap, BTreeSet};

use clew_core::{
    DeviceNameOrigin, DeviceRecord, DeviceTag, DeviceTagAllocationError, allocate_device_tag,
};
use thiserror::Error;

pub fn observed_hostname() -> String {
    let raw = hostname::get()
        .ok()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    normalize_hostname(&raw)
}

#[must_use]
pub fn normalize_hostname(raw: &str) -> String {
    let trimmed = raw.trim();
    let generic = trimmed.is_empty()
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "localhost" | "localhost.localdomain" | "computer" | "desktop" | "unknown"
        );
    if generic {
        fallback_platform_name().into()
    } else {
        trimmed.chars().take(128).collect()
    }
}

pub fn apply_hostname_collision_policy(
    records: &mut [DeviceRecord],
) -> Result<(), HostNamingError> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        if let DeviceNameOrigin::Automatic { base_hostname, .. } = &record.name_origin {
            groups
                .entry(base_hostname.to_ascii_lowercase())
                .or_default()
                .push(index);
        }
    }

    for indices in groups.values_mut() {
        indices.sort_by_key(|index| records[*index].device_id.to_string());
        let collision = indices.len() >= 2;
        let any_tagged = indices.iter().any(|index| {
            matches!(
                records[*index].name_origin,
                DeviceNameOrigin::Automatic { tagged: true, .. }
            )
        });
        if !collision && !any_tagged {
            continue;
        }

        let mut occupied = BTreeSet::<DeviceTag>::new();
        for index in indices.iter().copied() {
            let record = &mut records[index];
            let DeviceNameOrigin::Automatic {
                base_hostname,
                tagged,
                tag_generation,
            } = &mut record.name_origin
            else {
                continue;
            };

            let start_generation = if *tagged {
                tag_generation.saturating_add(1)
            } else {
                *tag_generation
            };
            let current = DeviceTag::derive(record.device_id, *tag_generation);
            let allocation = if *tagged && !occupied.contains(&current) {
                clew_core::DeviceTagAllocation {
                    tag: current,
                    generation: *tag_generation,
                }
            } else {
                allocate_device_tag(record.device_id, start_generation, |candidate| {
                    occupied.contains(candidate)
                })?
            };
            occupied.insert(allocation.tag);
            *tagged = true;
            *tag_generation = allocation.generation;
            record.display_name = format!("{base_hostname}-{}", allocation.tag);
        }
    }
    Ok(())
}

const fn fallback_platform_name() -> &'static str {
    #[cfg(windows)]
    {
        "Windows PC"
    }
    #[cfg(target_os = "macos")]
    {
        "Mac"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "Linux computer"
    }
}

#[derive(Debug, Error)]
pub enum HostNamingError {
    #[error(transparent)]
    DeviceTag(#[from] DeviceTagAllocationError),
}

#[cfg(test)]
mod tests {
    use clew_core::{DeviceId, InviteId, MemberCapabilities, SiteId};

    use super::*;

    fn record(id: &str, hostname: &str) -> DeviceRecord {
        DeviceRecord {
            device_id: id.parse().unwrap(),
            site_id: SiteId::new(),
            display_name: hostname.into(),
            hostname_observed: hostname.into(),
            capabilities: MemberCapabilities::EXECUTE_ONLY,
            enrolled_via_invite_id: InviteId::new(),
            name_origin: DeviceNameOrigin::Automatic {
                base_hostname: hostname.into(),
                tagged: false,
                tag_generation: 0,
            },
        }
    }

    #[test]
    fn hostname_collision_tags_entire_group_and_never_uses_sequence_suffix() {
        let mut records = vec![
            record("4b2bc107-8bd8-4c36-a5aa-7590dfde4f21", "GPU-01"),
            record("b815d61a-1f86-4e02-91d0-d78d82b6e112", "GPU-01"),
        ];
        apply_hostname_collision_policy(&mut records).unwrap();
        for record in &records {
            assert!(record.display_name.starts_with("GPU-01-"));
            assert_eq!(record.display_name.len(), "GPU-01-".len() + 5);
            assert!(!record.display_name.contains("(2)"));
        }
        assert_ne!(records[0].display_name, records[1].display_name);
    }

    #[test]
    fn tagged_name_stays_tagged_after_peer_disappears() {
        let mut records = vec![
            record("4b2bc107-8bd8-4c36-a5aa-7590dfde4f21", "GPU-01"),
            record("b815d61a-1f86-4e02-91d0-d78d82b6e112", "GPU-01"),
        ];
        apply_hostname_collision_policy(&mut records).unwrap();
        let expected = records[0].display_name.clone();
        records.truncate(1);
        apply_hostname_collision_policy(&mut records).unwrap();
        assert_eq!(records[0].display_name, expected);
    }

    #[test]
    fn renamed_device_is_not_retagged_by_automatic_collision() {
        let mut records = vec![
            record("4b2bc107-8bd8-4c36-a5aa-7590dfde4f21", "GPU-01"),
            record("b815d61a-1f86-4e02-91d0-d78d82b6e112", "GPU-01"),
        ];
        records[1].display_name = "Microscope PC".into();
        records[1].name_origin = DeviceNameOrigin::Renamed;
        apply_hostname_collision_policy(&mut records).unwrap();
        assert_eq!(records[0].display_name, "GPU-01");
        assert_eq!(records[1].display_name, "Microscope PC");
    }

    #[test]
    fn generic_hostname_uses_platform_human_name() {
        assert_eq!(normalize_hostname(" localhost "), fallback_platform_name());
        assert_ne!(normalize_hostname("GPU-01"), fallback_platform_name());
        let _: DeviceId = "4b2bc107-8bd8-4c36-a5aa-7590dfde4f21".parse().unwrap();
    }
}
