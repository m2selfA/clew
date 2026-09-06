use std::path::{Path, PathBuf};

use clew_core::HARD_MAX_READ_ROOT_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetPathError {
    UnsupportedHomeShorthand,
    HomeUnavailable,
    HomeNotAbsolute,
    ExpandedPathTooLong,
    HomePathNotUtf8,
}

pub(crate) fn expand_target_path(requested: &str) -> Result<PathBuf, TargetPathError> {
    expand_target_path_with_home(requested, target_account_home().as_deref())
}

fn expand_target_path_with_home(
    requested: &str,
    home: Option<&Path>,
) -> Result<PathBuf, TargetPathError> {
    let expanded = if requested == "~" {
        checked_home(home)?.to_path_buf()
    } else if let Some(relative) = requested.strip_prefix("~/") {
        join_home_relative(home, relative)?
    } else if cfg!(windows) {
        if let Some(relative) = requested.strip_prefix("~\\") {
            join_home_relative(home, relative)?
        } else if requested.starts_with('~') {
            return Err(TargetPathError::UnsupportedHomeShorthand);
        } else {
            PathBuf::from(requested)
        }
    } else if requested.starts_with('~') {
        return Err(TargetPathError::UnsupportedHomeShorthand);
    } else {
        PathBuf::from(requested)
    };

    let Some(text) = expanded.to_str() else {
        return Err(TargetPathError::HomePathNotUtf8);
    };
    if text.len() > HARD_MAX_READ_ROOT_BYTES {
        return Err(TargetPathError::ExpandedPathTooLong);
    }
    Ok(expanded)
}

fn join_home_relative(home: Option<&Path>, relative: &str) -> Result<PathBuf, TargetPathError> {
    if relative.starts_with('/') || relative.starts_with('\\') || Path::new(relative).is_absolute()
    {
        return Err(TargetPathError::UnsupportedHomeShorthand);
    }
    Ok(checked_home(home)?.join(relative))
}

fn checked_home(home: Option<&Path>) -> Result<&Path, TargetPathError> {
    let home = home.ok_or(TargetPathError::HomeUnavailable)?;
    if !home.is_absolute() {
        return Err(TargetPathError::HomeNotAbsolute);
    }
    if home.to_str().is_none() {
        return Err(TargetPathError::HomePathNotUtf8);
    }
    Ok(home)
}

#[cfg(windows)]
fn target_account_home() -> Option<PathBuf> {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let profile = PathBuf::from(profile);
        if profile.is_absolute() {
            return Some(profile);
        }
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    let mut combined = drive;
    combined.push(path);
    let home = PathBuf::from(combined);
    home.is_absolute().then_some(home)
}

#[cfg(not(windows))]
fn target_account_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn test_home() -> PathBuf {
        PathBuf::from(r"C:\Users\Alice")
    }

    #[cfg(not(windows))]
    fn test_home() -> PathBuf {
        PathBuf::from("/home/alice")
    }

    #[test]
    fn current_account_home_shorthand_is_narrow_and_deterministic() {
        let home = test_home();
        assert_eq!(
            expand_target_path_with_home("~", Some(&home)).unwrap(),
            home
        );
        assert_eq!(
            expand_target_path_with_home("~/.ssh/config", Some(&home)).unwrap(),
            home.join(".ssh/config")
        );
        assert_eq!(
            expand_target_path_with_home("relative/file", Some(&home)).unwrap(),
            PathBuf::from("relative/file")
        );
        assert_eq!(
            expand_target_path_with_home("~other/.ssh/config", Some(&home)),
            Err(TargetPathError::UnsupportedHomeShorthand)
        );
        assert_eq!(
            expand_target_path_with_home("~//escape", Some(&home)),
            Err(TargetPathError::UnsupportedHomeShorthand)
        );
        assert_eq!(
            expand_target_path_with_home("~/file", None),
            Err(TargetPathError::HomeUnavailable)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_backslash_home_shorthand_matches_slash_form() {
        let home = test_home();
        assert_eq!(
            expand_target_path_with_home(r"~\.ssh\config", Some(&home)).unwrap(),
            home.join(r".ssh\config")
        );
    }
}
