use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{ReadPolicy, TaskId};
use clew_transport::{
    HARD_MAX_SHELL_RETAINED_BYTES_PER_STREAM, HARD_MAX_SHELL_TASKS_PER_SESSION, ShellOutputChunk,
    ShellTaskErrorCode, ShellTaskOutput, ShellTaskPhase, ShellTaskReply, ShellTaskRequest,
    ShellTaskStatus,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinHandle,
};

#[derive(Clone, Debug)]
pub struct HostShellService {
    policy: ReadPolicy,
    inner: Arc<HostShellServiceInner>,
}

#[derive(Debug)]
struct HostShellServiceInner {
    tasks: Mutex<HostShellTaskStore>,
    capacity: Arc<Semaphore>,
}

impl Drop for HostShellServiceInner {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.get_mut() {
            for entry in tasks.tasks.values() {
                let _ = entry.cancel.send(true);
            }
        }
    }
}

#[derive(Debug, Default)]
struct HostShellTaskStore {
    next_sequence: u64,
    tasks: BTreeMap<TaskId, HostShellTaskEntry>,
}

#[derive(Debug)]
struct HostShellTaskEntry {
    sequence: u64,
    state: Arc<Mutex<HostShellTaskState>>,
    cancel: watch::Sender<bool>,
    _capacity_permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
struct HostShellTaskState {
    task_id: TaskId,
    phase: ShellTaskPhase,
    exit_code: Option<i32>,
    stdout: RetainedBytes,
    stderr: RetainedBytes,
}

impl HostShellTaskState {
    fn status(&self) -> ShellTaskStatus {
        ShellTaskStatus {
            task_id: self.task_id,
            phase: self.phase,
            exit_code: self.exit_code,
            stdout_base_offset: self.stdout.base_offset,
            stdout_next_offset: self.stdout.next_offset,
            stderr_base_offset: self.stderr.base_offset,
            stderr_next_offset: self.stderr.next_offset,
        }
    }
}

#[derive(Debug, Default)]
struct RetainedBytes {
    base_offset: u64,
    next_offset: u64,
    data: VecDeque<u8>,
}

impl RetainedBytes {
    fn append(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.data.push_back(byte);
            self.next_offset = self.next_offset.saturating_add(1);
            if self.data.len() > HARD_MAX_SHELL_RETAINED_BYTES_PER_STREAM {
                self.data.pop_front();
                self.base_offset = self.base_offset.saturating_add(1);
            }
        }
    }

    fn chunk(&self, requested_offset: u64, limit: usize) -> ShellOutputChunk {
        let start_offset = requested_offset.max(self.base_offset).min(self.next_offset);
        let start_index = start_offset.saturating_sub(self.base_offset) as usize;
        let data: Vec<u8> = self
            .data
            .iter()
            .skip(start_index)
            .take(limit)
            .copied()
            .collect();
        let next_offset = start_offset.saturating_add(data.len() as u64);
        ShellOutputChunk::from_bytes(
            requested_offset,
            start_offset,
            next_offset,
            self.base_offset,
            self.next_offset,
            requested_offset < self.base_offset,
            &data,
        )
        .expect("Host Shell ring enforces protocol output bounds")
    }
}

impl HostShellService {
    pub fn new(policy: ReadPolicy) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self {
            policy,
            inner: Arc::new(HostShellServiceInner {
                tasks: Mutex::new(HostShellTaskStore::default()),
                capacity: Arc::new(Semaphore::new(HARD_MAX_SHELL_TASKS_PER_SESSION)),
            }),
        })
    }

    pub async fn execute(&self, request: ShellTaskRequest, allow_shell: bool) -> ShellTaskReply {
        if request.validate().is_err() {
            return ShellTaskReply::error(
                ShellTaskErrorCode::InvalidRequest,
                "invalid bounded Shell task request",
            );
        }
        if !allow_shell {
            return ShellTaskReply::error(
                ShellTaskErrorCode::Denied,
                "Shell task is not permitted by this device grant",
            );
        }
        match request {
            ShellTaskRequest::Start {
                command,
                cwd,
                env,
                timeout_ms,
            } => self.start(command, cwd, env, timeout_ms).await,
            ShellTaskRequest::Status { task_id } => self.status(task_id),
            ShellTaskRequest::Attach {
                task_id,
                stdout_offset,
                stderr_offset,
                max_bytes_per_stream,
            } => self.attach(
                task_id,
                stdout_offset,
                stderr_offset,
                max_bytes_per_stream as usize,
            ),
            ShellTaskRequest::Cancel { task_id } => self.cancel(task_id),
        }
    }

    async fn start(
        &self,
        command: String,
        cwd: String,
        env: BTreeMap<String, String>,
        timeout_ms: u32,
    ) -> ShellTaskReply {
        let cwd = match self.canonical_allowed_cwd(Path::new(&cwd)).await {
            Ok(cwd) => cwd,
            Err(_) => {
                return ShellTaskReply::error(
                    ShellTaskErrorCode::Denied,
                    "Shell cwd is outside the signed roots or is not a directory",
                );
            }
        };

        {
            let mut store = match self.inner.tasks.lock() {
                Ok(store) => store,
                Err(_) => {
                    return ShellTaskReply::error(
                        ShellTaskErrorCode::Io,
                        "Shell task store is unavailable",
                    );
                }
            };
            prune_terminal_tasks(&mut store);
        }
        let capacity_permit = match Arc::clone(&self.inner.capacity).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return ShellTaskReply::error(
                    ShellTaskErrorCode::Capacity,
                    "Shell task capacity is exhausted for this live session",
                );
            }
        };

        let mut child = match spawn_shell(&command, &cwd, &env) {
            Ok(child) => child,
            Err(_) => {
                return ShellTaskReply::error(
                    ShellTaskErrorCode::SpawnFailed,
                    "Shell process could not be started",
                );
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.start_kill();
            return ShellTaskReply::error(
                ShellTaskErrorCode::SpawnFailed,
                "Shell stdout pipe was not created",
            );
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.start_kill();
            return ShellTaskReply::error(
                ShellTaskErrorCode::SpawnFailed,
                "Shell stderr pipe was not created",
            );
        };

        let task_id = TaskId::new();
        let state = Arc::new(Mutex::new(HostShellTaskState {
            task_id,
            phase: ShellTaskPhase::Running,
            exit_code: None,
            stdout: RetainedBytes::default(),
            stderr: RetainedBytes::default(),
        }));
        let (cancel, cancel_rx) = watch::channel(false);

        let mut store = match self.inner.tasks.lock() {
            Ok(store) => store,
            Err(_) => {
                let _ = child.start_kill();
                return ShellTaskReply::error(
                    ShellTaskErrorCode::Io,
                    "Shell task store is unavailable",
                );
            }
        };
        store.next_sequence = store.next_sequence.saturating_add(1);
        let sequence = store.next_sequence;
        store.tasks.insert(
            task_id,
            HostShellTaskEntry {
                sequence,
                state: Arc::clone(&state),
                cancel,
                _capacity_permit: capacity_permit,
            },
        );
        drop(store);

        let stdout_task = tokio::spawn(drain_pipe(stdout, Arc::clone(&state), ShellStream::Stdout));
        let stderr_task = tokio::spawn(drain_pipe(stderr, Arc::clone(&state), ShellStream::Stderr));
        tokio::spawn(run_child(
            child,
            cancel_rx,
            Duration::from_millis(timeout_ms as u64),
            state,
            stdout_task,
            stderr_task,
        ));

        ShellTaskReply::Started { task_id }
    }

    fn status(&self, task_id: TaskId) -> ShellTaskReply {
        let state = match self.task_state(task_id) {
            Ok(state) => state,
            Err(reply) => return reply,
        };
        match state.lock() {
            Ok(state) => ShellTaskReply::Status(state.status()),
            Err(_) => {
                ShellTaskReply::error(ShellTaskErrorCode::Io, "Shell task state is unavailable")
            }
        }
    }

    fn attach(
        &self,
        task_id: TaskId,
        stdout_offset: u64,
        stderr_offset: u64,
        max_bytes_per_stream: usize,
    ) -> ShellTaskReply {
        let state = match self.task_state(task_id) {
            Ok(state) => state,
            Err(reply) => return reply,
        };
        match state.lock() {
            Ok(state) => ShellTaskReply::Output(ShellTaskOutput {
                status: state.status(),
                stdout: state.stdout.chunk(stdout_offset, max_bytes_per_stream),
                stderr: state.stderr.chunk(stderr_offset, max_bytes_per_stream),
            }),
            Err(_) => {
                ShellTaskReply::error(ShellTaskErrorCode::Io, "Shell task state is unavailable")
            }
        }
    }

    fn cancel(&self, task_id: TaskId) -> ShellTaskReply {
        let cancel = {
            let store = match self.inner.tasks.lock() {
                Ok(store) => store,
                Err(_) => {
                    return ShellTaskReply::error(
                        ShellTaskErrorCode::Io,
                        "Shell task store is unavailable",
                    );
                }
            };
            let Some(entry) = store.tasks.get(&task_id) else {
                return ShellTaskReply::error(
                    ShellTaskErrorCode::NotFound,
                    "Shell task was not found",
                );
            };
            entry.cancel.clone()
        };
        let _ = cancel.send(true);
        ShellTaskReply::CancelAccepted { task_id }
    }

    fn task_state(
        &self,
        task_id: TaskId,
    ) -> Result<Arc<Mutex<HostShellTaskState>>, ShellTaskReply> {
        let store = self.inner.tasks.lock().map_err(|_| {
            ShellTaskReply::error(ShellTaskErrorCode::Io, "Shell task store is unavailable")
        })?;
        store
            .tasks
            .get(&task_id)
            .map(|entry| Arc::clone(&entry.state))
            .ok_or_else(|| {
                ShellTaskReply::error(ShellTaskErrorCode::NotFound, "Shell task was not found")
            })
    }

    async fn canonical_allowed_cwd(&self, requested: &Path) -> Result<PathBuf, ()> {
        if !requested.is_absolute() {
            return Err(());
        }
        let cwd = tokio::fs::canonicalize(requested).await.map_err(|_| ())?;
        let metadata = tokio::fs::metadata(&cwd).await.map_err(|_| ())?;
        if !metadata.is_dir() {
            return Err(());
        }
        for root in &self.policy.roots {
            let Ok(root) = tokio::fs::canonicalize(root).await else {
                continue;
            };
            if cwd.starts_with(root) {
                return Ok(cwd);
            }
        }
        Err(())
    }
}

fn prune_terminal_tasks(store: &mut HostShellTaskStore) {
    while store.tasks.len() >= HARD_MAX_SHELL_TASKS_PER_SESSION {
        let candidate = store
            .tasks
            .iter()
            .filter_map(|(task_id, entry)| {
                let terminal = entry
                    .state
                    .lock()
                    .map(|state| state.phase.terminal())
                    .unwrap_or(false);
                terminal.then_some((*task_id, entry.sequence))
            })
            .min_by_key(|(_, sequence)| *sequence)
            .map(|(task_id, _)| task_id);
        let Some(task_id) = candidate else {
            break;
        };
        store.tasks.remove(&task_id);
    }
}

#[derive(Clone, Copy)]
enum ShellStream {
    Stdout,
    Stderr,
}

async fn drain_pipe<R>(
    mut pipe: R,
    state: Arc<Mutex<HostShellTaskState>>,
    stream: ShellStream,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        let mut state = state
            .lock()
            .map_err(|_| std::io::Error::other("Shell task state is poisoned"))?;
        match stream {
            ShellStream::Stdout => state.stdout.append(&buffer[..read]),
            ShellStream::Stderr => state.stderr.append(&buffer[..read]),
        }
    }
}

async fn run_child(
    mut child: Child,
    mut cancel: watch::Receiver<bool>,
    timeout: Duration,
    state: Arc<Mutex<HostShellTaskState>>,
    stdout_task: JoinHandle<std::io::Result<()>>,
    stderr_task: JoinHandle<std::io::Result<()>>,
) {
    let timeout_sleep = tokio::time::sleep(timeout);
    tokio::pin!(timeout_sleep);
    let (phase, exit_code) = tokio::select! {
        result = child.wait() => match result {
            Ok(status) => (ShellTaskPhase::Exited, status.code()),
            Err(_) => (ShellTaskPhase::Failed, None),
        },
        changed = cancel.changed() => {
            let _ = changed;
            let _ = child.kill().await;
            (ShellTaskPhase::Cancelled, None)
        },
        _ = &mut timeout_sleep => {
            let _ = child.kill().await;
            (ShellTaskPhase::TimedOut, None)
        }
    };

    if phase == ShellTaskPhase::Exited {
        let stdout_ok = matches!(stdout_task.await, Ok(Ok(())));
        let stderr_ok = matches!(stderr_task.await, Ok(Ok(())));
        if let Ok(mut state) = state.lock() {
            if stdout_ok && stderr_ok {
                state.phase = ShellTaskPhase::Exited;
                state.exit_code = exit_code;
            } else {
                state.phase = ShellTaskPhase::Failed;
                state.exit_code = None;
            }
        }
    } else {
        stdout_task.abort();
        stderr_task.abort();
        if let Ok(mut state) = state.lock() {
            state.phase = phase;
            state.exit_code = exit_code;
        }
    }
}

fn spawn_shell(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Child> {
    let mut process = platform_shell_command(command)?;
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for key in minimal_environment_keys() {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
    process.envs(env);
    process.spawn()
}

#[cfg(windows)]
fn platform_shell_command(command: &str) -> std::io::Result<Command> {
    let shell = std::env::var_os("ComSpec")
        .filter(|value| Path::new(value).is_absolute())
        .or_else(|| {
            std::env::var_os("SystemRoot").map(|root| {
                PathBuf::from(root)
                    .join("System32")
                    .join("cmd.exe")
                    .into_os_string()
            })
        })
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "cmd.exe was not found")
        })?;
    let mut process = Command::new(shell);
    process.args(["/D", "/S", "/C", command]);
    Ok(process)
}

#[cfg(not(windows))]
fn platform_shell_command(command: &str) -> std::io::Result<Command> {
    let shell = Path::new("/bin/sh");
    if !shell.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "/bin/sh was not found",
        ));
    }
    let mut process = Command::new(shell);
    process.args(["-c", command]);
    Ok(process)
}

#[cfg(windows)]
fn minimal_environment_keys() -> &'static [&'static str] {
    &[
        "PATH",
        "PATHEXT",
        "SystemRoot",
        "WINDIR",
        "ComSpec",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ]
}

#[cfg(not(windows))]
fn minimal_environment_keys() -> &'static [&'static str] {
    &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_transport::HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM;
    use tempfile::tempdir;

    #[test]
    fn retained_ring_reports_lost_prefix_and_absolute_cursors() {
        let mut retained = RetainedBytes::default();
        retained.append(&vec![b'x'; HARD_MAX_SHELL_RETAINED_BYTES_PER_STREAM + 5]);
        assert_eq!(retained.base_offset, 5);
        let chunk = retained.chunk(0, 16);
        assert!(chunk.lost_prefix);
        assert_eq!(chunk.start_offset, 5);
        assert_eq!(chunk.next_offset, 21);
        assert_eq!(chunk.decode().unwrap(), vec![b'x'; 16]);
    }

    #[tokio::test]
    async fn live_shell_task_has_bounded_output_status_timeout_cancel_and_root_policy() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        std::fs::create_dir_all(&root).unwrap();
        let service = HostShellService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 49_152, 5_000).unwrap(),
        )
        .unwrap();

        let denied = service
            .execute(
                ShellTaskRequest::start(
                    success_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                false,
            )
            .await;
        assert!(matches!(
            denied,
            ShellTaskReply::Error(error) if error.code == ShellTaskErrorCode::Denied
        ));

        let outside = service
            .execute(
                ShellTaskRequest::start(
                    success_command(),
                    temp.path().to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            outside,
            ShellTaskReply::Error(error) if error.code == ShellTaskErrorCode::Denied
        ));

        let started = service
            .execute(
                ShellTaskRequest::start(
                    success_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                true,
            )
            .await;
        let ShellTaskReply::Started { task_id } = started else {
            panic!("expected Shell task start");
        };
        let status = wait_terminal(&service, task_id).await;
        assert_eq!(status.phase, ShellTaskPhase::Exited);
        assert_eq!(status.exit_code, Some(0));
        let output = service
            .execute(
                ShellTaskRequest::Attach {
                    task_id,
                    stdout_offset: 0,
                    stderr_offset: 0,
                    max_bytes_per_stream: HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM,
                },
                true,
            )
            .await;
        let ShellTaskReply::Output(output) = output else {
            panic!("expected Shell output");
        };
        assert!(String::from_utf8_lossy(&output.stdout.decode().unwrap()).contains("CLEW-OUT"));
        assert!(String::from_utf8_lossy(&output.stderr.decode().unwrap()).contains("CLEW-ERR"));

        let timed = service
            .execute(
                ShellTaskRequest::start(
                    wait_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    100,
                )
                .unwrap(),
                true,
            )
            .await;
        let ShellTaskReply::Started { task_id: timed_id } = timed else {
            panic!("expected timed Shell task start");
        };
        assert_eq!(
            wait_terminal(&service, timed_id).await.phase,
            ShellTaskPhase::TimedOut
        );

        let cancellable = service
            .execute(
                ShellTaskRequest::start(
                    wait_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                true,
            )
            .await;
        let ShellTaskReply::Started { task_id: cancel_id } = cancellable else {
            panic!("expected cancellable Shell task start");
        };
        assert!(matches!(
            service
                .execute(ShellTaskRequest::Cancel { task_id: cancel_id }, true)
                .await,
            ShellTaskReply::CancelAccepted { .. }
        ));
        assert_eq!(
            wait_terminal(&service, cancel_id).await.phase,
            ShellTaskPhase::Cancelled
        );
    }

    #[tokio::test]
    async fn live_shell_capacity_is_reserved_before_spawn_and_service_drop_cancels_tasks() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        std::fs::create_dir_all(&root).unwrap();
        let service = HostShellService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 49_152, 5_000).unwrap(),
        )
        .unwrap();

        let mut permits = Vec::with_capacity(HARD_MAX_SHELL_TASKS_PER_SESSION);
        for _ in 0..HARD_MAX_SHELL_TASKS_PER_SESSION {
            permits.push(
                Arc::clone(&service.inner.capacity)
                    .try_acquire_owned()
                    .expect("test should reserve every Shell capacity permit"),
            );
        }
        let capacity = service
            .execute(
                ShellTaskRequest::start(
                    success_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            capacity,
            ShellTaskReply::Error(error) if error.code == ShellTaskErrorCode::Capacity
        ));
        drop(permits);

        let mut env = BTreeMap::new();
        env.insert("CLEW_EXPLICIT_ENV".to_owned(), "VISIBLE".to_owned());
        let started = service
            .execute(
                ShellTaskRequest::start(env_command(), root.to_string_lossy(), env, 5_000).unwrap(),
                true,
            )
            .await;
        let ShellTaskReply::Started { task_id: env_id } = started else {
            panic!("expected Shell task start with explicit env");
        };
        let env_status = wait_terminal(&service, env_id).await;
        assert_eq!(env_status.phase, ShellTaskPhase::Exited);
        let ShellTaskReply::Output(env_output) = service
            .execute(
                ShellTaskRequest::Attach {
                    task_id: env_id,
                    stdout_offset: 0,
                    stderr_offset: 0,
                    max_bytes_per_stream: HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM,
                },
                true,
            )
            .await
        else {
            panic!("expected explicit env Shell output");
        };
        assert!(String::from_utf8_lossy(&env_output.stdout.decode().unwrap()).contains("VISIBLE"));

        let running = service
            .execute(
                ShellTaskRequest::start(
                    wait_command(),
                    root.to_string_lossy(),
                    BTreeMap::new(),
                    5_000,
                )
                .unwrap(),
                true,
            )
            .await;
        let ShellTaskReply::Started {
            task_id: running_id,
        } = running
        else {
            panic!("expected live Shell task before service drop");
        };
        let running_state = service.task_state(running_id).unwrap();
        drop(service);
        let phase = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let phase = running_state.lock().unwrap().phase;
                if phase.terminal() {
                    break phase;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping the live-session Shell service should cancel its child");
        assert_eq!(phase, ShellTaskPhase::Cancelled);
    }

    async fn wait_terminal(service: &HostShellService, task_id: TaskId) -> ShellTaskStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ShellTaskReply::Status(status) = service
                    .execute(ShellTaskRequest::Status { task_id }, true)
                    .await
                else {
                    panic!("expected Shell task status");
                };
                if status.phase.terminal() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Shell task did not reach terminal state")
    }

    #[cfg(windows)]
    fn success_command() -> &'static str {
        "echo CLEW-OUT & echo CLEW-ERR 1>&2"
    }

    #[cfg(not(windows))]
    fn success_command() -> &'static str {
        "printf CLEW-OUT; printf CLEW-ERR >&2"
    }

    #[cfg(windows)]
    fn env_command() -> &'static str {
        "echo %CLEW_EXPLICIT_ENV%"
    }

    #[cfg(not(windows))]
    fn env_command() -> &'static str {
        "printf %s \"$CLEW_EXPLICIT_ENV\""
    }

    #[cfg(windows)]
    fn wait_command() -> &'static str {
        "for /L %i in (1,1,2147483647) do @ver >NUL"
    }

    #[cfg(not(windows))]
    fn wait_command() -> &'static str {
        "while :; do :; done"
    }
}
