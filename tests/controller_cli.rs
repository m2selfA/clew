use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::tempdir;

const READY_TIMEOUT: Duration = Duration::from_secs(10);

struct ChildGuard(Child);

impl ChildGuard {
    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if self.0.try_wait().unwrap().is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "controller did not exit after shutdown"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn run_status(state_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clew"))
        .arg("status")
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
}

fn run_devices(state_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clew"))
        .arg("devices")
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
}

fn run_read(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("read");
    command.args(operands);
    command.arg("--state-dir").arg(state_dir).output().unwrap()
}

fn run_shutdown(state_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clew"))
        .arg("shutdown")
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
}

fn wait_until_ready(state_dir: &Path) -> Output {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let output = run_status(state_dir);
        if output.status.success() {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "controller did not become ready: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn spawn_controller(state_dir: &Path) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_clew"))
            .arg("controller")
            .arg("--state-dir")
            .arg(state_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn authenticated_cli_shutdown_stops_controller_and_allows_restart() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();

    let mut first = spawn_controller(state_dir);
    let first_status = wait_until_ready(state_dir);
    let first_json: serde_json::Value = serde_json::from_slice(&first_status.stdout).unwrap();
    let first_instance = first_json["instance_id"].as_str().unwrap().to_owned();
    let first_controller_id = first_json["controller_id"].as_str().unwrap().to_owned();

    let shutdown = run_shutdown(state_dir);
    assert!(
        shutdown.status.success(),
        "{}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    first.wait_for_exit();

    let _restarted = spawn_controller(state_dir);
    let restarted_status = wait_until_ready(state_dir);
    let restarted_json: serde_json::Value =
        serde_json::from_slice(&restarted_status.stdout).unwrap();
    assert_ne!(
        restarted_json["instance_id"].as_str().unwrap(),
        first_instance
    );
    assert_eq!(
        restarted_json["controller_id"].as_str().unwrap(),
        first_controller_id
    );
}

#[test]
fn read_cli_accepts_shared_device_selector_or_single_device_auto_selection() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();
    let _controller = spawn_controller(state_dir);
    wait_until_ready(state_dir);

    let automatic = run_read(state_dir, &["proof.txt"]);
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let named = run_read(state_dir, &["GPU-01", "proof.txt"]);
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("device selector not found: GPU-01"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );
}

#[test]
fn second_controller_becomes_client_and_crash_recovery_reclaims_ownership() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();

    let mut first = spawn_controller(state_dir);
    let first_status = wait_until_ready(state_dir);
    let first_json: serde_json::Value = serde_json::from_slice(&first_status.stdout).unwrap();
    let first_instance = first_json["instance_id"].as_str().unwrap().to_owned();
    let first_controller_id = first_json["controller_id"].as_str().unwrap().to_owned();

    let devices = run_devices(state_dir);
    assert!(
        devices.status.success(),
        "{}",
        String::from_utf8_lossy(&devices.stderr)
    );
    let devices_json: serde_json::Value = serde_json::from_slice(&devices.stdout).unwrap();
    assert_eq!(devices_json["devices"].as_array().unwrap().len(), 0);

    let second = Command::new(env!("CARGO_BIN_EXE_clew"))
        .arg("controller")
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap();
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

    let _recovered = spawn_controller(state_dir);
    let recovered_status = wait_until_ready(state_dir);
    let recovered_json: serde_json::Value =
        serde_json::from_slice(&recovered_status.stdout).unwrap();
    let recovered_instance = recovered_json["instance_id"].as_str().unwrap();
    assert_ne!(recovered_instance, first_instance);
    assert_eq!(
        recovered_json["controller_id"].as_str().unwrap(),
        first_controller_id
    );
}
