use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use clew_core::TaskId;
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

fn run_path_info(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("path-info");
    command.args(operands);
    command.arg("--state-dir").arg(state_dir).output().unwrap()
}

fn run_glob(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("glob");
    command.args(operands);
    command.arg("--state-dir").arg(state_dir).output().unwrap()
}

fn run_grep(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("grep");
    command.args(operands);
    command.arg("--state-dir").arg(state_dir).output().unwrap()
}

fn run_write(state_dir: &Path, operands: &[&str], precondition_args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("write");
    command.args(operands);
    command.arg("--contents").arg("hello");
    command.args(precondition_args);
    command.arg("--state-dir").arg(state_dir).output().unwrap()
}

fn run_edit(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("edit");
    command.args(operands);
    command
        .arg("--expected-sha256")
        .arg("00".repeat(32))
        .arg("--old")
        .arg("old")
        .arg("--new")
        .arg("new")
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
}

fn run_shell_start(state_dir: &Path, operands: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clew"));
    command.arg("shell").arg("start");
    command.args(operands);
    command
        .arg("--cwd")
        .arg(state_dir)
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
}

fn run_shell_followup(state_dir: &Path, operation: &str, task_id: TaskId) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clew"))
        .arg("shell")
        .arg(operation)
        .arg(task_id.to_string())
        .arg("--state-dir")
        .arg(state_dir)
        .output()
        .unwrap()
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
fn path_info_and_glob_cli_reuse_shared_device_selection() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();
    let _controller = spawn_controller(state_dir);
    wait_until_ready(state_dir);

    let info = run_path_info(state_dir, &["proof.txt"]);
    assert!(!info.status.success());
    assert!(
        String::from_utf8_lossy(&info.stderr).contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );

    let glob = run_glob(state_dir, &["/shared", "**/*.rs"]);
    assert!(!glob.status.success());
    assert!(
        String::from_utf8_lossy(&glob.stderr).contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&glob.stderr)
    );

    let named = run_glob(state_dir, &["GPU-01", "/shared", "**/*.rs"]);
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("device selector not found: GPU-01"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );
}

#[test]
fn grep_cli_reuses_shared_device_selection() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();
    let _controller = spawn_controller(state_dir);
    wait_until_ready(state_dir);

    let automatic = run_grep(state_dir, &["/shared", "TODO"]);
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let named = run_grep(state_dir, &["GPU-01", "/shared", "TODO"]);
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("device selector not found: GPU-01"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );
}

#[test]
fn write_and_edit_cli_reuse_shared_device_selection_and_require_preconditions() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();
    let _controller = spawn_controller(state_dir);
    wait_until_ready(state_dir);

    let invalid = run_write(state_dir, &["/shared/new.txt"], &[]);
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .contains("write requires exactly one of --create-only or --expected-sha256"),
        "{}",
        String::from_utf8_lossy(&invalid.stderr)
    );

    let automatic = run_write(state_dir, &["/shared/new.txt"], &["--create-only"]);
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let named = run_write(
        state_dir,
        &["GPU-01", "/shared/new.txt"],
        &["--create-only"],
    );
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("device selector not found: GPU-01"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );

    let edit = run_edit(state_dir, &["/shared/new.txt"]);
    assert!(!edit.status.success());
    assert!(
        String::from_utf8_lossy(&edit.stderr).contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&edit.stderr)
    );
}

#[test]
fn shell_cli_uses_selector_only_for_start_and_task_projection_for_followups() {
    let temp = tempdir().unwrap();
    let state_dir = temp.path();
    let _controller = spawn_controller(state_dir);
    wait_until_ready(state_dir);

    let automatic = run_shell_start(state_dir, &["echo automatic"]);
    assert!(!automatic.status.success());
    assert!(
        String::from_utf8_lossy(&automatic.stderr)
            .contains("no online executable device is available"),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );

    let named = run_shell_start(state_dir, &["GPU-01", "echo named"]);
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("device selector not found: GPU-01"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );

    let unknown = TaskId::new();
    for operation in ["status", "attach", "cancel"] {
        let output = run_shell_followup(state_dir, operation, unknown);
        assert!(
            !output.status.success(),
            "{operation} unexpectedly succeeded"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Shell task is not available in the current live session"),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
