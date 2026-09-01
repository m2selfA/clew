use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use clew_core::{InviteId, MemberCapabilities, SiteId, StateLayout};
use clew_host::{ClientFlavor, HostInstanceKey, HostRoleHint, SignedSiteClew};
use clew_identity::{
    ControllerIdentity, DeviceIdentityStore, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
};
use tempfile::tempdir;

const READY_TIMEOUT: Duration = Duration::from_secs(10);

struct ChildGuard(Child);

impl ChildGuard {
    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn assert_running(&mut self) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "Host exited unexpectedly"
        );
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

struct Fixture {
    state: PathBuf,
    sidecar: PathBuf,
    controller: ControllerIdentity,
    site_id: SiteId,
}

fn fixture(root: &Path) -> Fixture {
    let state = root.join("state");
    let kit = root.join("kit");
    std::fs::create_dir_all(&kit).unwrap();
    let controller = ControllerIdentity::from_secret([101_u8; 32]);
    let site_id = SiteId::new();
    let invite_id = InviteId::new();
    let mut registry = EnrollmentRegistry::new(
        controller.controller_id(),
        PermissionGrant {
            member: MemberCapabilities::EXECUTE_ONLY,
            read: true,
            write: false,
            shell: false,
        },
    );
    let bootstrap = registry
        .issue_bootstrap(
            &controller,
            SiteBootstrapSpec {
                site_id,
                invite_id,
                site_name: "CLI Smoke Lab".into(),
                grant: PermissionGrant::EXECUTE_READ,
                not_before_unix_ms: 1,
                expires_unix_ms: u64::MAX - 1,
                deployment_window_ms: 60_000,
                max_claims: 4,
            },
        )
        .unwrap();
    let site_file = SignedSiteClew::issue(
        &controller,
        ClientFlavor::clew_original_current(),
        bootstrap,
        HostRoleHint::ExecutePreferred,
    )
    .unwrap();
    let sidecar = kit.join("site.clew");
    site_file.write(&sidecar).unwrap();
    Fixture {
        state,
        sidecar,
        controller,
        site_id,
    }
}

fn spawn_host(fixture: &Fixture, foreground: bool) -> ChildGuard {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command
        .arg("host")
        .arg("--site")
        .arg(&fixture.sidecar)
        .arg("--state-dir")
        .arg(&fixture.state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if foreground {
        command.arg("--foreground");
    }
    ChildGuard(command.spawn().unwrap())
}

fn second_host(fixture: &Fixture, foreground: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command
        .arg("host")
        .arg("--site")
        .arg(&fixture.sidecar)
        .arg("--state-dir")
        .arg(&fixture.state);
    if foreground {
        command.arg("--foreground");
    }
    command.output().unwrap()
}

fn wait_for_host_ready(fixture: &Fixture) {
    let layout = StateLayout::new(&fixture.state);
    let pending =
        layout.pending_device_identity_path(fixture.controller.controller_id(), fixture.site_id);
    let key = HostInstanceKey::membership(fixture.controller.controller_id(), fixture.site_id);
    let runtime_secret = layout
        .version_root()
        .join("host-runtime")
        .join(key.path_component())
        .join("runtime.secret");
    let deadline = Instant::now() + READY_TIMEOUT;
    while !pending.is_file() || !runtime_secret.is_file() {
        assert!(
            Instant::now() < deadline,
            "Host did not reach single-instance ready state"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn second_foreground_host_wakes_existing_and_restart_reuses_device_key() {
    let temp = tempdir().unwrap();
    let fixture = fixture(temp.path());
    let mut first = spawn_host(&fixture, true);
    wait_for_host_ready(&fixture);
    first.assert_running();
    let store = DeviceIdentityStore::new(StateLayout::new(&fixture.state));
    let first_public = store
        .load_pending(fixture.controller.controller_id(), fixture.site_id)
        .unwrap()
        .unwrap()
        .public_identity();

    let second = second_host(&fixture, true);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("already running"),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );

    first.kill_and_wait();
    let mut restarted = spawn_host(&fixture, true);
    wait_for_host_ready(&fixture);
    restarted.assert_running();
    let restarted_public = store
        .load_pending(fixture.controller.controller_id(), fixture.site_id)
        .unwrap()
        .unwrap()
        .public_identity();
    assert_eq!(restarted_public, first_public);
}

#[cfg(windows)]
#[test]
#[ignore = "requires an interactive Windows desktop session"]
fn windows_host_gui_and_tray_smoke() {
    let temp = tempdir().unwrap();
    let fixture = fixture(temp.path());
    let mut first = spawn_host(&fixture, false);
    wait_for_host_ready(&fixture);
    thread::sleep(Duration::from_millis(750));
    first.assert_running();

    let second = second_host(&fixture, false);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("already running"));
    first.assert_running();
}
