use clew_core::{DeviceId, DeviceSummary};
use thiserror::Error;

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
    use clew_core::SiteId;

    use super::*;

    fn summary(name: &str, executable: bool, online: bool) -> DeviceSummary {
        DeviceSummary {
            device_id: DeviceId::new(),
            site_id: SiteId::new(),
            site_name: "Alice Lab".into(),
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
        let helper = summary("Helper", false, true);
        let target = summary("GPU-01", true, true);
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
    fn ambiguous_short_name_returns_candidates_instead_of_first() {
        let first = summary("GPU-01", true, true);
        let mut second = summary("GPU-01", true, true);
        second.site_name = "Bob Lab".into();
        let devices = vec![first, second];
        assert!(matches!(
            select_executable_device(&devices, Some("GPU-01")),
            Err(DeviceSelectionError::Ambiguous(candidates)) if candidates.len() == 2
        ));
    }
}
