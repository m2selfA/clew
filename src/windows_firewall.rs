#![cfg(windows)]

use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sha2::{Digest, Sha256};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RULE_PREFIX: &str = "Clew Nearby Helper";

pub fn ensure_current_helper_firewall() -> Result<(), String> {
    let executable = current_executable()?;
    let rule_name = helper_rule_name(&executable);
    if helper_rule_matches(&rule_name, &executable)? {
        return Ok(());
    }
    elevate_self_for_install()?;
    if helper_rule_matches(&rule_name, &executable)? {
        Ok(())
    } else {
        Err(
            "Windows Firewall did not expose the expected Clew helper UDP rule after elevation"
                .into(),
        )
    }
}

pub fn install_current_helper_firewall() -> Result<(), String> {
    let executable = current_executable()?;
    let rule_name = helper_rule_name(&executable);
    let _ = netsh(&[
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &format!("name={rule_name}"),
    ]);
    let status = netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &format!("name={rule_name}"),
        "dir=in",
        "action=allow",
        "enable=yes",
        "profile=any",
        &format!("program={}", executable.display()),
        "protocol=UDP",
        "localport=any",
        "remoteip=localsubnet",
    ])?;
    if !status.success() {
        return Err(format!(
            "Windows Firewall rejected the Clew helper rule (netsh exit {})",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

fn helper_rule_matches(rule_name: &str, executable: &Path) -> Result<bool, String> {
    const QUERY: &str = r#"
$r = Get-NetFirewallRule -DisplayName $env:CLEW_FW_RULE -PolicyStore ActiveStore -ErrorAction SilentlyContinue |
    Where-Object { $_.Enabled -eq 'True' -and $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' } |
    Select-Object -First 1
if ($null -eq $r) { exit 1 }
$af = $r | Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue
$pf = $r | Get-NetFirewallPortFilter -ErrorAction SilentlyContinue
$addr = $r | Get-NetFirewallAddressFilter -ErrorAction SilentlyContinue
if ($null -eq $af -or $null -eq $pf -or $null -eq $addr) { exit 1 }
if ($af.Program -ine $env:CLEW_FW_PROGRAM) { exit 1 }
if (("$($pf.Protocol)" -ne 'UDP') -and ("$($pf.Protocol)" -ne '17')) { exit 1 }
if ("$($pf.LocalPort)" -ne 'Any') { exit 1 }
if (-not ($addr.RemoteAddress -contains 'LocalSubnet')) { exit 1 }
if ("$($r.Profile)" -ne 'Any') { exit 1 }
exit 0
"#;
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            QUERY,
        ])
        .env("CLEW_FW_RULE", rule_name)
        .env("CLEW_FW_PROGRAM", executable)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("could not query Windows Firewall: {error}"))?;
    Ok(status.success())
}

fn elevate_self_for_install() -> Result<(), String> {
    const ELEVATE: &str = r#"
try {
    $p = Start-Process -FilePath $env:CLEW_FW_EXE -ArgumentList 'windows-helper-firewall-install' -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ErrorAction Stop
    exit $p.ExitCode
} catch {
    exit 1223
}
"#;
    let executable = current_executable()?;
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            ELEVATE,
        ])
        .env("CLEW_FW_EXE", &executable)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("could not request Windows Firewall elevation: {error}"))?;
    if status.success() {
        Ok(())
    } else if status.code() == Some(1223) {
        Err("Windows Firewall permission was not granted; helper C cannot accept nearby Target connections"
            .into())
    } else {
        Err(format!(
            "elevated Windows Firewall setup failed (exit {})",
            status.code().unwrap_or(-1)
        ))
    }
}

fn netsh(args: &[&str]) -> Result<std::process::ExitStatus, String> {
    Command::new("netsh.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("could not run Windows Firewall netsh command: {error}"))
}

fn current_executable() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("could not resolve Clew runtime path: {error}"))
}

fn helper_rule_name(executable: &Path) -> String {
    let normalized = executable.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    let suffix = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{RULE_PREFIX} {suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_rule_name_is_stable_path_scoped_and_bounded() {
        let first = helper_rule_name(Path::new(r"C:\Clew A\.clew-runtime\clew.exe"));
        let same_case = helper_rule_name(Path::new(r"c:\clew a\.CLEW-RUNTIME\CLEW.EXE"));
        let second = helper_rule_name(Path::new(r"C:\Clew B\.clew-runtime\clew.exe"));
        assert_eq!(first, same_case);
        assert_ne!(first, second);
        assert!(first.starts_with(RULE_PREFIX));
        assert!(first.len() < 80);
    }
}
