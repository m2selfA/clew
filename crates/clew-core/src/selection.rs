use thiserror::Error;

use crate::{DeviceId, DeviceSummary};

pub fn select_executable_device(
    devices: &[DeviceSummary],
    selector: Option<&str>,
) -> Result<DeviceId, DeviceSelectionError> {
    let online_executable: Vec<_> = devices
        .iter()
        .filter(|device| device.online && device.executable)
        .collect();

    let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) else {
        return match online_executable.as_slice() {
            [only] => Ok(only.device_id),
            [] => Err(DeviceSelectionError::NoOnlineExecutableDevice),
            many => Err(DeviceSelectionError::Ambiguous(
                many.iter().map(|device| candidate(device)).collect(),
            )),
        };
    };

    if let Ok(device_id) = selector.parse::<DeviceId>() {
        let device = devices
            .iter()
            .find(|device| device.device_id == device_id)
            .ok_or_else(|| DeviceSelectionError::NotFound(selector.into()))?;
        if !device.executable {
            return Err(DeviceSelectionError::NotExecutable(device_id));
        }
        if !device.online {
            return Err(DeviceSelectionError::Offline(device_id));
        }
        return Ok(device_id);
    }

    if let Some((site, name)) = selector.split_once('/') {
        let matches: Vec<_> = online_executable
            .iter()
            .filter(|device| device.site_name == site && device.display_name == name)
            .collect();
        return unique_match(selector, &matches);
    }

    let matches: Vec<_> = online_executable
        .iter()
        .filter(|device| device.display_name == selector)
        .collect();
    unique_match(selector, &matches)
}

fn unique_match(
    selector: &str,
    matches: &[&&DeviceSummary],
) -> Result<DeviceId, DeviceSelectionError> {
    match matches {
        [only] => Ok(only.device_id),
        [] => Err(DeviceSelectionError::NotFound(selector.into())),
        many => Err(DeviceSelectionError::Ambiguous(
            many.iter().map(|device| candidate(device)).collect(),
        )),
    }
}

fn candidate(device: &DeviceSummary) -> String {
    format!(
        "{}/{} [{}]",
        device.site_name, device.display_name, device.device_id
    )
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DeviceSelectionError {
    #[error("no online executable device is available")]
    NoOnlineExecutableDevice,
    #[error("device selector is ambiguous; candidates: {0:?}")]
    Ambiguous(Vec<String>),
    #[error("device selector not found: {0}")]
    NotFound(String),
    #[error("device {0} is helper-only and cannot execute tools")]
    NotExecutable(DeviceId),
    #[error("device {0} is offline")]
    Offline(DeviceId),
}

#[cfg(test)]
mod tests {
    use crate::SiteId;

    use super::*;

    fn summary(site: &str, name: &str, executable: bool, online: bool) -> DeviceSummary {
        DeviceSummary {
            device_id: DeviceId::new(),
            site_id: SiteId::new(),
            site_name: site.into(),
            display_name: name.into(),
            hostname_observed: name.into(),
            online,
            executable,
            connector: !executable,
            last_seen_unix_ms: None,
        }
    }

    #[test]
    fn helper_only_is_never_an_executable_candidate() {
        let helper = summary("Alice Lab", "Helper", false, true);
        let target = summary("Alice Lab", "GPU-01", true, true);
        let devices = vec![helper.clone(), target.clone()];
        assert_eq!(
            select_executable_device(&devices, None).unwrap(),
            target.device_id
        );
        assert!(matches!(
            select_executable_device(&devices, Some(&helper.device_id.to_string())),
            Err(DeviceSelectionError::NotExecutable(id)) if id == helper.device_id
        ));
    }

    #[test]
    fn qualified_and_unique_short_names_resolve_but_duplicates_do_not() {
        let first = summary("Alice Lab", "GPU-01", true, true);
        let second = summary("Bob Lab", "GPU-01", true, true);
        let unique = summary("Alice Lab", "CPU-01", true, true);
        let devices = vec![first.clone(), second, unique.clone()];
        assert_eq!(
            select_executable_device(&devices, Some("Alice Lab/GPU-01")).unwrap(),
            first.device_id
        );
        assert_eq!(
            select_executable_device(&devices, Some("CPU-01")).unwrap(),
            unique.device_id
        );
        assert!(matches!(
            select_executable_device(&devices, Some("GPU-01")),
            Err(DeviceSelectionError::Ambiguous(candidates)) if candidates.len() == 2
        ));
    }

    #[test]
    fn omitted_selector_requires_exactly_one_online_executable_device() {
        let first = summary("Alice Lab", "GPU-01", true, true);
        let second = summary("Bob Lab", "GPU-02", true, true);
        assert!(matches!(
            select_executable_device(&[first.clone(), second], None),
            Err(DeviceSelectionError::Ambiguous(candidates)) if candidates.len() == 2
        ));
        let mut offline = first;
        offline.online = false;
        assert!(matches!(
            select_executable_device(&[offline], None),
            Err(DeviceSelectionError::NoOnlineExecutableDevice)
        ));
    }
}
