use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    process::ExitStatus,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use crate::garm::ProviderRequest;

use super::{
    AmbiguousOutcome, GARM_INTERFACE_VERSION, LocalFailure, ProcessLimits, ProviderBinding,
    ProviderError,
};

pub(super) struct ChildOutput {
    pub(super) stdout: Vec<u8>,
    pub(super) _stderr: Vec<u8>,
    pub(super) status: ExitStatus,
}

struct ProcessGroupGuard {
    process_id: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(process_id: u32) -> Self {
        Self {
            process_id: Some(process_id),
        }
    }

    fn stop(&self) {
        if let Some(process_id) = self.process_id {
            #[cfg(unix)]
            // SAFETY: the child was started in its own process group; a negative pid targets it.
            unsafe {
                libc::kill(-(process_id as i32), libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.process_id = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ReaderTasks {
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
    stdin: JoinHandle<()>,
}

struct ReaderResults {
    stdout: tokio::sync::oneshot::Receiver<Result<Vec<u8>, ProviderError>>,
    stderr: tokio::sync::oneshot::Receiver<Result<Vec<u8>, ProviderError>>,
    stdin: tokio::sync::oneshot::Receiver<Result<(), ProviderError>>,
}

struct ProcessCancellation {
    sender: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ProcessCancellation {
    fn disarm(&mut self) {
        self.sender = None;
    }
}

impl Drop for ProcessCancellation {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _send_result = sender.send(());
        }
    }
}

impl ReaderTasks {
    fn abort(&self) {
        self.stdout.abort();
        self.stderr.abort();
        self.stdin.abort();
    }
}

impl Drop for ReaderTasks {
    fn drop(&mut self) {
        self.abort();
    }
}

pub(super) async fn run_process(
    binding: &ProviderBinding,
    request: &ProviderRequest,
    environment_snapshot: &BTreeMap<OsString, OsString>,
    limits: ProcessLimits,
    deadline: Instant,
) -> Result<ChildOutput, ProviderError> {
    if Instant::now() >= deadline {
        return Err(LocalFailure::Deadline.into());
    }

    let input = if let Some(bootstrap) = request.bootstrap.as_ref() {
        serde_json::to_vec(bootstrap).map_err(|_| LocalFailure::InvalidResult)?
    } else {
        Vec::new()
    };

    if input.len() > limits.stdin_bytes {
        return Err(LocalFailure::InputLimit.into());
    }

    if Instant::now() >= deadline {
        return Err(LocalFailure::Deadline.into());
    }

    let mut command = Command::new(&binding.executable_path);
    command
        .env_clear()
        .env("GARM_COMMAND", &request.command)
        .env("GARM_CONTROLLER_ID", &request.controller_id)
        .env("GARM_PROVIDER_CONFIG_FILE", &binding.configuration_path)
        .env("GARM_INTERFACE_VERSION", GARM_INTERFACE_VERSION)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(pool_id) = request.pool_id.as_ref() {
        command.env("GARM_POOL_ID", pool_id);
    }

    if let Some(instance_id) = request.instance_id.as_ref() {
        command.env("GARM_INSTANCE_ID", instance_id);
    }

    for name in &binding.inherited_environment {
        let key = OsStr::new(name);
        if let Some(value) = environment_snapshot.get(key) {
            command.env(key, value);
        }
    }

    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(|_| LocalFailure::Spawn)?;
    let result = supervise_spawned_process(child, input, limits, deadline).await;
    if request.command == "CreateInstance" {
        result.map_err(|_| AmbiguousOutcome.into())
    } else {
        result
    }
}

async fn supervise_spawned_process(
    mut child: tokio::process::Child,
    input: Vec<u8>,
    limits: ProcessLimits,
    deadline: Instant,
) -> Result<ChildOutput, ProviderError> {
    let process_id = child.id().ok_or(LocalFailure::Supervision)?;
    let process_group = ProcessGroupGuard::new(process_id);
    let stdin = child.stdin.take().ok_or(LocalFailure::Supervision)?;
    let stdout = child.stdout.take().ok_or(LocalFailure::Supervision)?;
    let stderr = child.stderr.take().ok_or(LocalFailure::Supervision)?;
    let (stdout_sender, stdout_receiver) = tokio::sync::oneshot::channel();
    let (stderr_sender, stderr_receiver) = tokio::sync::oneshot::channel();
    let (stdin_sender, stdin_receiver) = tokio::sync::oneshot::channel();
    let tasks = ReaderTasks {
        stdout: tokio::spawn(async move {
            let _send_result = stdout_sender.send(read_bounded(stdout, limits.stdout_bytes).await);
        }),
        stderr: tokio::spawn(async move {
            let _send_result = stderr_sender.send(read_bounded(stderr, limits.stderr_bytes).await);
        }),
        stdin: tokio::spawn(async move {
            let _send_result = stdin_sender.send(write_input(stdin, input).await);
        }),
    };

    let results = ReaderResults {
        stdout: stdout_receiver,
        stderr: stderr_receiver,
        stdin: stdin_receiver,
    };

    let (cancel_sender, cancel_receiver) = tokio::sync::oneshot::channel();
    let mut cancellation = ProcessCancellation {
        sender: Some(cancel_sender),
    };

    let supervisor = tokio::spawn(supervise_process(
        child,
        process_group,
        tasks,
        results,
        cancel_receiver,
        deadline,
    ));
    let result = supervisor.await.map_err(|_| LocalFailure::Supervision)?;
    cancellation.disarm();
    result
}

async fn supervise_process(
    mut child: tokio::process::Child,
    mut process_group: ProcessGroupGuard,
    mut tasks: ReaderTasks,
    mut results: ReaderResults,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
    deadline: Instant,
) -> Result<ChildOutput, ProviderError> {
    let completion = timeout_at(deadline, async {
        tokio::select! {
            result = async {
                tokio::try_join!(
                    async {
                        child
                            .wait()
                            .await
                            .map_err(|_| ProviderError::Local(LocalFailure::Supervision))
                    },
                    join_reader(&mut results.stdout),
                    join_reader(&mut results.stderr),
                    join_writer(&mut results.stdin),
                )
            } => result,
            _ = &mut cancelled => Err(ProviderError::Local(LocalFailure::Supervision)),
        }
    })
    .await;

    match completion {
        Ok(Ok((status, stdout, stderr, ()))) => {
            if join_tasks(&mut tasks, false).await.is_err() {
                process_group.stop();
                return Err(LocalFailure::Supervision.into());
            }

            process_group.stop();
            process_group.disarm();
            Ok(ChildOutput {
                stdout,
                _stderr: stderr,
                status,
            })
        }

        Ok(Err(error)) => {
            let cleanup = stop_and_reap(&mut child, &process_group, tasks).await;
            if cleanup.is_err() {
                return Err(LocalFailure::Supervision.into());
            }

            Err(error)
        }

        Err(_) => {
            let cleanup = stop_and_reap(&mut child, &process_group, tasks).await;
            if cleanup.is_err() {
                return Err(LocalFailure::Supervision.into());
            }

            Err(LocalFailure::Deadline.into())
        }
    }
}

async fn stop_and_reap(
    child: &mut tokio::process::Child,
    process_group: &ProcessGroupGuard,
    mut tasks: ReaderTasks,
) -> Result<(), ProviderError> {
    tasks.abort();
    process_group.stop();
    let wait_result = child.wait().await;
    join_tasks(&mut tasks, true).await?;
    wait_result
        .map(|_| ())
        .map_err(|_| LocalFailure::Supervision.into())
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, ProviderError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| LocalFailure::Supervision)?;
        if count == 0 {
            return Ok(output);
        }

        if output.len().saturating_add(count) > limit {
            return Err(LocalFailure::OutputLimit.into());
        }

        output.extend_from_slice(&chunk[..count]);
    }
}

async fn write_input<W>(mut writer: W, input: Vec<u8>) -> Result<(), ProviderError>
where
    W: AsyncWriteExt + Unpin,
{
    writer
        .write_all(&input)
        .await
        .map_err(|_| LocalFailure::Supervision)?;
    writer
        .shutdown()
        .await
        .map_err(|_| LocalFailure::Supervision)?;
    Ok(())
}

async fn join_reader(
    receiver: &mut tokio::sync::oneshot::Receiver<Result<Vec<u8>, ProviderError>>,
) -> Result<Vec<u8>, ProviderError> {
    receiver.await.map_err(|_| LocalFailure::Supervision)?
}

async fn join_writer(
    receiver: &mut tokio::sync::oneshot::Receiver<Result<(), ProviderError>>,
) -> Result<(), ProviderError> {
    receiver.await.map_err(|_| LocalFailure::Supervision)?
}

async fn join_tasks(tasks: &mut ReaderTasks, cancellation_ok: bool) -> Result<(), ProviderError> {
    for task in [&mut tasks.stdout, &mut tasks.stderr, &mut tasks.stdin] {
        if let Err(error) = task.await
            && (!cancellation_ok || !error.is_cancelled())
        {
            return Err(LocalFailure::Supervision.into());
        }
    }

    Ok(())
}
