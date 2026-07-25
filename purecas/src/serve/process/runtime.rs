use super::config::ProcessRoute;
use axum::body::{Body, Bytes};
use axum::response::{IntoResponse, Response};
use futures_util::{future, StreamExt};
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderValue, StatusCode};
use rustix::process::{kill_process_group, set_parent_process_death_signal, Pid, Signal};
use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, timeout, Instant};

const IO_CHUNK_BYTES: usize = 16 * 1024;
const BODY_CHANNEL_CAPACITY: usize = 2;
const STDERR_TAIL_BYTES: usize = 64 * 1024;
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

enum Start {
    Streaming(StreamState),
    Empty,
    Error(StatusCode, String),
}

enum Terminal {
    Clean,
    Error(String),
}

struct StreamState {
    data: mpsc::Receiver<Bytes>,
    terminal: Option<oneshot::Receiver<Terminal>>,
}

#[derive(Debug)]
enum InputFailure {
    TooLarge,
    Read(String),
    Write(io::Error),
}

impl std::fmt::Display for InputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("request body exceeds configured maximum"),
            Self::Read(message) => write!(f, "reading request body: {message}"),
            Self::Write(error) => write!(f, "writing child stdin: {error}"),
        }
    }
}

#[derive(Debug)]
enum Failure {
    ClientGone,
    Input(InputFailure),
    Stdout(io::Error),
    Stderr(io::Error),
    Timeout,
    Shutdown,
    Internal(String),
}

struct PumpHandles<'a> {
    stdin: &'a mut JoinHandle<Result<(), InputFailure>>,
    stdout: &'a mut JoinHandle<io::Result<()>>,
    stderr: &'a mut JoinHandle<io::Result<Vec<u8>>>,
}

#[derive(Clone, Copy)]
struct PumpCompletion {
    stdin: bool,
    stdout: bool,
    stderr: bool,
}

struct ProcessGuard {
    pid: Pid,
    route: Arc<ProcessRoute>,
    armed: bool,
}

impl ProcessGuard {
    fn new(pid: Pid, route: Arc<ProcessRoute>) -> Self {
        route.register_group(pid);
        Self {
            pid,
            route,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.route.unregister_group(self.pid);
        self.armed = false;
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_process_group(self.pid, Signal::KILL);
            self.route.unregister_group(self.pid);
        }
    }
}

impl Failure {
    fn status(&self) -> StatusCode {
        match self {
            Self::Input(InputFailure::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Input(InputFailure::Read(_)) => StatusCode::BAD_REQUEST,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Shutdown => StatusCode::SERVICE_UNAVAILABLE,
            Self::Input(InputFailure::Write(_))
            | Self::Stdout(_)
            | Self::Stderr(_)
            | Self::Internal(_) => StatusCode::BAD_GATEWAY,
            Self::ClientGone => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::ClientGone => "HTTP client disconnected".to_string(),
            Self::Input(error) => error.to_string(),
            Self::Stdout(error) => format!("reading child stdout: {error}"),
            Self::Stderr(error) => format!("reading child stderr: {error}"),
            Self::Timeout => "process route timed out".to_string(),
            Self::Shutdown => "server is shutting down".to_string(),
            Self::Internal(message) => message.clone(),
        }
    }
}

pub(crate) async fn execute(
    route: Arc<ProcessRoute>,
    argv: Vec<OsString>,
    body: Body,
    permit: OwnedSemaphorePermit,
) -> Response {
    let response_content_type = route.response_content_type();
    let (start_tx, start_rx) = oneshot::channel();
    tokio::spawn(supervise(route, argv, body, start_tx, permit));

    match start_rx.await {
        Ok(Start::Streaming(state)) => response_with_body(
            StatusCode::OK,
            response_content_type,
            Body::from_stream(futures_util::stream::unfold(
                state,
                |mut state| async move {
                    if let Some(bytes) = state.data.recv().await {
                        return Some((Ok::<_, io::Error>(bytes), state));
                    }
                    let terminal = state.terminal.take()?;
                    match terminal.await {
                        Ok(Terminal::Error(message)) => {
                            Some((Err(io::Error::other(message)), state))
                        }
                        Ok(Terminal::Clean) | Err(_) => None,
                    }
                },
            )),
        ),
        Ok(Start::Empty) => {
            response_with_body(StatusCode::OK, response_content_type, Body::empty())
        }
        Ok(Start::Error(status, message)) => (
            status,
            [(CONTENT_TYPE, "text/plain; charset=utf-8")],
            message,
        )
            .into_response(),
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "process supervisor terminated before producing a response",
        )
            .into_response(),
    }
}

pub(crate) fn unsupported_media_type() -> Response {
    (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Media Type").into_response()
}

pub(crate) fn payload_too_large() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large").into_response()
}

pub(crate) fn saturated() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        "Process route concurrency limit reached",
    )
        .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

fn response_with_body(status: StatusCode, content_type: HeaderValue, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    response
}

async fn supervise(
    route: Arc<ProcessRoute>,
    argv: Vec<OsString>,
    body: Body,
    start_tx: oneshot::Sender<Start>,
    _permit: OwnedSemaphorePermit,
) {
    // Creating timer resources before spawning ensures a runtime without a
    // time driver cannot leave a child behind after a timer panic.
    let deadline = Instant::now() + route.timeout();
    let mut child_poll = tokio::time::interval(Duration::from_millis(10));
    child_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut command = Command::new(route.executable());
    command
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    unsafe {
        command.pre_exec(|| {
            set_parent_process_death_signal(Some(Signal::KILL))
                .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = start_tx.send(Start::Error(
                StatusCode::BAD_GATEWAY,
                format!("spawning configured executable: {error}"),
            ));
            return;
        }
    };
    let Some(pid) = child.id().and_then(|raw| Pid::from_raw(raw as i32)) else {
        let _ = start_tx.send(Start::Error(
            StatusCode::BAD_GATEWAY,
            "spawned child has no process ID".to_string(),
        ));
        let _ = child.start_kill();
        let _ = child.wait().await;
        return;
    };
    let mut process_guard = ProcessGuard::new(pid, Arc::clone(&route));
    let Some(stdin) = child.stdin.take() else {
        let _ = start_tx.send(Start::Error(
            StatusCode::BAD_GATEWAY,
            "spawned child has no stdin pipe".to_string(),
        ));
        terminate_and_reap(pid, &mut child, &mut process_guard).await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = start_tx.send(Start::Error(
            StatusCode::BAD_GATEWAY,
            "spawned child has no stdout pipe".to_string(),
        ));
        terminate_and_reap(pid, &mut child, &mut process_guard).await;
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = start_tx.send(Start::Error(
            StatusCode::BAD_GATEWAY,
            "spawned child has no stderr pipe".to_string(),
        ));
        terminate_and_reap(pid, &mut child, &mut process_guard).await;
        return;
    };

    let (body_tx, body_rx) = mpsc::channel(BODY_CHANNEL_CAPACITY);
    let (terminal_tx, terminal_rx) = oneshot::channel();
    let (first_tx, mut first_rx) = oneshot::channel();
    let mut stdin_task = tokio::spawn(pump_stdin(body, stdin, route.max_request_bytes()));
    let mut stdout_task = tokio::spawn(pump_stdout(stdout, first_tx, body_tx.clone()));
    let mut stderr_task = tokio::spawn(pump_stderr(stderr));
    let mut start_tx = Some(start_tx);
    let mut terminal_tx = Some(terminal_tx);
    let mut body_rx = Some(body_rx);
    let mut terminal_rx = Some(terminal_rx);
    let mut committed = false;
    let mut first_done = false;
    let mut stdin_done = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stderr_tail = Vec::new();
    let mut child_status = None;
    let failure = loop {
        if first_done && stdin_done && stdout_done && stderr_done {
            break None;
        }
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => break Some(Failure::Timeout),
            _ = route.cancelled() => break Some(Failure::Shutdown),
            _ = async {
                if let Some(sender) = start_tx.as_mut() {
                    sender.closed().await;
                } else {
                    future::pending::<()>().await;
                }
            }, if !committed => break Some(Failure::ClientGone),
            _ = body_tx.closed(), if committed => break Some(Failure::ClientGone),
            _ = child_poll.tick(), if child_status.is_none() => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = kill_process_group(pid, Signal::KILL);
                        child_status = Some(status);
                    }
                    Ok(None) => {}
                    Err(error) => break Some(Failure::Internal(format!("polling child status: {error}"))),
                }
            }
            first = &mut first_rx, if !first_done => {
                first_done = true;
                if first.is_ok() {
                    let receiver = body_rx.take().expect("body receiver is available before commit");
                    let terminal = terminal_rx
                        .take()
                        .expect("terminal receiver is available before commit");
                    let sender = start_tx.take().expect("start sender is available before commit");
                    if sender
                        .send(Start::Streaming(StreamState {
                            data: receiver,
                            terminal: Some(terminal),
                        }))
                        .is_err()
                    {
                        break Some(Failure::ClientGone);
                    }
                    committed = true;
                }
            }
            result = &mut stdin_task, if !stdin_done => {
                stdin_done = true;
                match join_result(result, "stdin pump") {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break Some(Failure::Input(error)),
                    Err(error) => break Some(error),
                }
            }
            result = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
                match join_result(result, "stdout pump") {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break Some(Failure::Stdout(error)),
                    Err(error) => break Some(error),
                }
            }
            result = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
                match join_result(result, "stderr pump") {
                    Ok(Ok(tail)) => stderr_tail = tail,
                    Ok(Err(error)) => break Some(Failure::Stderr(error)),
                    Err(error) => break Some(error),
                }
            }
        }
    };

    if let Some(failure) = failure {
        terminate_tasks_and_reap(
            pid,
            &mut child,
            PumpHandles {
                stdin: &mut stdin_task,
                stdout: &mut stdout_task,
                stderr: &mut stderr_task,
            },
            PumpCompletion {
                stdin: stdin_done,
                stdout: stdout_done,
                stderr: stderr_done,
            },
            &mut process_guard,
        )
        .await;
        drop(body_tx);
        finish_failure(
            failure,
            committed,
            &mut start_tx,
            &mut terminal_tx,
            &stderr_tail,
        )
        .await;
        return;
    }

    let status = if let Some(status) = child_status {
        status
    } else {
        tokio::select! {
                biased;
                _ = sleep_until(deadline) => {
                    terminate_tasks_and_reap(
                        pid,
                        &mut child,
                        PumpHandles { stdin: &mut stdin_task, stdout: &mut stdout_task, stderr: &mut stderr_task },
                        PumpCompletion { stdin: stdin_done, stdout: stdout_done, stderr: stderr_done },
                        &mut process_guard,
                    ).await;
                    drop(body_tx);
                    finish_failure(Failure::Timeout, committed, &mut start_tx, &mut terminal_tx, &stderr_tail).await;
                    return;
                }
                _ = route.cancelled() => {
                    terminate_tasks_and_reap(
                        pid,
                        &mut child,
                        PumpHandles { stdin: &mut stdin_task, stdout: &mut stdout_task, stderr: &mut stderr_task },
                        PumpCompletion { stdin: stdin_done, stdout: stdout_done, stderr: stderr_done },
                        &mut process_guard,
                    ).await;
                    drop(body_tx);
                    finish_failure(Failure::Shutdown, committed, &mut start_tx, &mut terminal_tx, &stderr_tail).await;
                    return;
                }
                _ = async {
                    if let Some(sender) = start_tx.as_mut() {
                        sender.closed().await;
                    } else {
                        future::pending::<()>().await;
                    }
                }, if !committed => {
                    terminate_tasks_and_reap(
                        pid,
                        &mut child,
                        PumpHandles { stdin: &mut stdin_task, stdout: &mut stdout_task, stderr: &mut stderr_task },
                        PumpCompletion { stdin: stdin_done, stdout: stdout_done, stderr: stderr_done },
                        &mut process_guard,
                    ).await;
                    return;
                }
                _ = body_tx.closed(), if committed => {
                    terminate_tasks_and_reap(
                        pid,
                        &mut child,
                        PumpHandles { stdin: &mut stdin_task, stdout: &mut stdout_task, stderr: &mut stderr_task },
                        PumpCompletion { stdin: stdin_done, stdout: stdout_done, stderr: stderr_done },
                        &mut process_guard,
                    ).await;
                    return;
                }
                result = child.wait() => match result {
                    Ok(status) => status,
                    Err(error) => {
                        terminate_tasks_and_reap(
                            pid,
                            &mut child,
                            PumpHandles { stdin: &mut stdin_task, stdout: &mut stdout_task, stderr: &mut stderr_task },
                            PumpCompletion { stdin: stdin_done, stdout: stdout_done, stderr: stderr_done },
                            &mut process_guard,
                        ).await;
                        drop(body_tx);
                        finish_failure(
                            Failure::Internal(format!("waiting for child: {error}")),
                            committed,
                            &mut start_tx,
                            &mut terminal_tx,
                            &stderr_tail,
                        ).await;
                        return;
                    }
                }
        }
    };
    // The direct child status is terminal for the producer. Any remaining
    // group members are descendants and must not outlive the route request.
    let _ = kill_process_group(pid, Signal::KILL);
    process_guard.disarm();

    if committed {
        drop(body_tx);
        if !status.success() {
            let message = exit_message(status, &stderr_tail);
            if let Some(sender) = terminal_tx.take() {
                let _ = sender.send(Terminal::Error(message));
            }
        } else if let Some(sender) = terminal_tx.take() {
            let _ = sender.send(Terminal::Clean);
        }
    } else {
        let sender = start_tx
            .take()
            .expect("uncommitted response has a start sender");
        if status.success() {
            let _ = sender.send(Start::Empty);
        } else {
            let _ = sender.send(Start::Error(
                StatusCode::BAD_GATEWAY,
                exit_message(status, &stderr_tail),
            ));
        }
    }
}

fn join_result<T>(
    result: Result<T, tokio::task::JoinError>,
    task: &'static str,
) -> Result<T, Failure> {
    result.map_err(|error| Failure::Internal(format!("{task} failed: {error}")))
}

async fn pump_stdin(
    body: Body,
    mut stdin: ChildStdin,
    max_request_bytes: u64,
) -> Result<(), InputFailure> {
    let mut received = 0_u64;
    let mut stdin_closed = false;
    let mut fatal_write_error = None;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| InputFailure::Read(error.to_string()))?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or(InputFailure::TooLarge)?;
        if received > max_request_bytes {
            return Err(InputFailure::TooLarge);
        }
        if !stdin_closed && fatal_write_error.is_none() {
            if let Err(error) = stdin.write_all(&chunk).await {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    stdin_closed = true;
                } else {
                    fatal_write_error = Some(error);
                }
            }
        }
    }
    if let Some(error) = fatal_write_error {
        return Err(InputFailure::Write(error));
    }
    if stdin_closed {
        return Ok(());
    }
    if let Err(error) = stdin.flush().await {
        return if error.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(InputFailure::Write(error))
        };
    }
    match stdin.shutdown().await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(InputFailure::Write(error)),
    }
}

async fn pump_stdout(
    mut stdout: ChildStdout,
    first_tx: oneshot::Sender<()>,
    body_tx: mpsc::Sender<Bytes>,
) -> io::Result<()> {
    let mut first_tx = Some(first_tx);
    let mut buffer = vec![0_u8; IO_CHUNK_BYTES];
    loop {
        let read = stdout.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        if let Some(sender) = first_tx.take() {
            let _ = sender.send(());
        }
        body_tx
            .send(Bytes::copy_from_slice(&buffer[..read]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "HTTP response was dropped"))?;
    }
}

async fn pump_stderr(mut stderr: ChildStderr) -> io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut buffer = vec![0_u8; IO_CHUNK_BYTES];
    loop {
        let read = stderr.read(&mut buffer).await?;
        if read == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&buffer[..read]);
        if tail.len() > STDERR_TAIL_BYTES {
            tail.drain(..tail.len() - STDERR_TAIL_BYTES);
        }
    }
}

async fn finish_failure(
    failure: Failure,
    committed: bool,
    start_tx: &mut Option<oneshot::Sender<Start>>,
    terminal_tx: &mut Option<oneshot::Sender<Terminal>>,
    stderr_tail: &[u8],
) {
    if matches!(failure, Failure::ClientGone) {
        return;
    }
    let mut message = failure.message();
    append_stderr(&mut message, stderr_tail);
    if committed {
        if let Some(sender) = terminal_tx.take() {
            let _ = sender.send(Terminal::Error(message));
        }
    } else if let Some(sender) = start_tx.take() {
        let _ = sender.send(Start::Error(failure.status(), message));
    }
}

fn exit_message(status: ExitStatus, stderr_tail: &[u8]) -> String {
    let mut message = match (status.code(), status.signal()) {
        (Some(code), _) => format!("child exited with status {code}"),
        (None, Some(signal)) => format!("child terminated by signal {signal}"),
        _ => "child terminated abnormally".to_string(),
    };
    append_stderr(&mut message, stderr_tail);
    message
}

fn append_stderr(message: &mut String, stderr_tail: &[u8]) {
    if !stderr_tail.is_empty() {
        message.push_str("\nstderr tail:\n");
        message.push_str(&String::from_utf8_lossy(stderr_tail));
    }
}

async fn terminate_tasks_and_reap(
    pid: Pid,
    child: &mut Child,
    tasks: PumpHandles<'_>,
    done: PumpCompletion,
    guard: &mut ProcessGuard,
) {
    let _ = kill_process_group(pid, Signal::KILL);
    if !done.stdin {
        tasks.stdin.abort();
        let _ = tasks.stdin.await;
    }
    if !done.stdout {
        tasks.stdout.abort();
        let _ = tasks.stdout.await;
    }
    if !done.stderr {
        tasks.stderr.abort();
        let _ = tasks.stderr.await;
    }
    let _ = timeout(REAP_TIMEOUT, child.wait()).await;
    guard.disarm();
}

async fn terminate_and_reap(pid: Pid, child: &mut Child, guard: &mut ProcessGuard) {
    let _ = kill_process_group(pid, Signal::KILL);
    let _ = timeout(REAP_TIMEOUT, child.wait()).await;
    guard.disarm();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::process::ProcessRoutes;
    use http_body_util::BodyExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    fn fixture() -> &'static Path {
        static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let output = std::env::temp_dir()
                    .join(format!("purecas-process-fixture-{}", std::process::id()));
                let source =
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process_fixture.rs");
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
                let status = std::process::Command::new(rustc)
                    .arg(&source)
                    .arg("-O")
                    .arg("-o")
                    .arg(&output)
                    .status()
                    .expect("running rustc for process fixture");
                assert!(status.success(), "compiling {}", source.display());
                output
            })
            .as_path()
    }

    fn configured(
        mode: &str,
        max_request_bytes: u64,
        max_concurrency: usize,
        timeout_seconds: u64,
    ) -> (Arc<ProcessRoute>, Vec<OsString>) {
        configured_args(
            &[mode.to_string()],
            max_request_bytes,
            max_concurrency,
            timeout_seconds,
        )
    }

    fn configured_args(
        args: &[String],
        max_request_bytes: u64,
        max_concurrency: usize,
        timeout_seconds: u64,
    ) -> (Arc<ProcessRoute>, Vec<OsString>) {
        let args = args
            .iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            r#"
[[process_routes]]
path = "/run"
executable = {executable:?}
args = [{args}]
request_content_type = "application/octet-stream"
response_content_type = "application/octet-stream"
max_request_bytes = {max_request_bytes}
max_concurrency = {max_concurrency}
timeout_seconds = {timeout_seconds}
"#,
            executable = fixture().to_string_lossy(),
        );
        ProcessRoutes::parse(&source)
            .unwrap()
            .match_path("/run")
            .unwrap()
            .into_parts()
    }

    async fn run(mode: &str, body: Body, max: u64, timeout_seconds: u64) -> Response {
        let (route, argv) = configured(mode, max, 1, timeout_seconds);
        let permit = route.try_acquire().unwrap();
        execute(route, argv, body, permit).await
    }

    async fn run_args(args: &[String], body: Body, max: u64, timeout_seconds: u64) -> Response {
        let (route, argv) = configured_args(args, max, 1, timeout_seconds);
        let permit = route.try_acquire().unwrap();
        execute(route, argv, body, permit).await
    }

    #[tokio::test]
    async fn closes_stdin_at_exact_size_boundary_before_child_responds() {
        let response = run("eof", Body::from("abcde"), 5, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "eof:5:abcde"
        );
    }

    #[tokio::test]
    async fn request_overflow_kills_child_before_response_commit() {
        let response = run("eof", Body::from("abcdef"), 5, 5).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .starts_with(b"request body exceeds"));
    }

    #[tokio::test]
    async fn child_exit_does_not_bypass_delayed_request_overflow() {
        let body = Body::from_stream(futures_util::stream::unfold(0, |state| async move {
            match state {
                0 => Some((Ok::<_, std::io::Error>(Bytes::from_static(b"abc")), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from_static(b"def")), 2))
                }
                _ => None,
            }
        }));
        let response = run("exit", body, 5, 5).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn zero_exit_with_closed_stdin_accepts_in_bounds_body() {
        let body = Body::from_stream(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))
        }));
        let response = run("close-stdin-empty", body, 5, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty());
    }

    #[tokio::test]
    async fn successful_output_survives_benign_stdin_epipe() {
        let body = Body::from_stream(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(b"leftover"))
        }));
        let response = run("close-stdin-output", body, 8, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"accepted".as_slice()
        );
    }

    #[tokio::test]
    async fn closed_stdin_still_enforces_delayed_size_overflow() {
        let body = Body::from_stream(futures_util::stream::unfold(0, |state| async move {
            match state {
                0 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from_static(b"abc")), 1))
                }
                1 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from_static(b"def")), 2))
                }
                _ => None,
            }
        }));
        let response = run("close-stdin-empty", body, 5, 5).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn interleaved_large_read_write_does_not_deadlock() {
        let input = vec![0x5a; 1024 * 1024];
        let response = run(
            "interleave",
            Body::from(input.clone()),
            input.len() as u64,
            10,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            input
        );
    }

    #[tokio::test]
    async fn slow_bidirectional_backpressure_remains_bounded() {
        let input = vec![0x33; 1024];
        let response = run("slow", Body::from(input.clone()), input.len() as u64, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            input
        );
    }

    #[tokio::test]
    async fn trusted_literal_metacharacters_are_one_argv_value_without_shell() {
        let marker = std::env::temp_dir().join(format!(
            "purecas-process-shell-marker-{}",
            std::process::id()
        ));
        let metacharacters = format!("$(touch {}) ; --host evil", marker.display());
        let response = run_args(
            &["argv".to_string(), metacharacters.clone()],
            Body::empty(),
            1,
            5,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            format!("{metacharacters}\n")
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn early_nonzero_is_502_with_bounded_stderr() {
        let response = run("stderr-flood", Body::empty(), 1, 5).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.len() <= STDERR_TAIL_BYTES + 128, "{}", body.len());
        assert!(body.ends_with(&vec![b'e'; STDERR_TAIL_BYTES]));
    }

    #[tokio::test]
    async fn early_nonzero_kills_pipe_holding_descendant_before_502() {
        let response = run("early-fail-descendant", Body::empty(), 1, 5).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .windows(b"early fixture failure".len())
            .any(|window| window == b"early fixture failure"));
    }

    #[tokio::test]
    async fn zero_exit_kills_pipe_holding_descendant_before_empty_200() {
        let response = run("exit-descendant-inherit", Body::empty(), 1, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty());
    }

    #[tokio::test]
    async fn late_nonzero_turns_stream_eof_into_body_error() {
        let response = run("late-fail", Body::empty(), 1, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.into_body().collect().await.is_err());
    }

    #[tokio::test]
    async fn zero_exit_without_output_is_clean_empty_200() {
        let response = run("empty", Body::from("ignored"), 7, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty());
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_before_504() {
        let response = run("sleep", Body::empty(), 1, 1).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn timeout_releases_permit_when_response_channel_is_full() {
        let (route, argv) = configured("flood-sleep", 1, 1, 1);
        let permit = route.try_acquire().unwrap();
        let response = execute(Arc::clone(&route), argv, Body::empty(), permit).await;
        assert_eq!(response.status(), StatusCode::OK);

        tokio::time::sleep(Duration::from_millis(1200)).await;

        assert!(
            route.try_acquire().is_ok(),
            "terminal error delivery must not retain the permit"
        );
        drop(response);
    }

    #[test]
    fn runtime_without_time_driver_fails_before_spawning_child() {
        let pid_file =
            std::env::temp_dir().join(format!("purecas-no-time-child-{}", std::process::id()));
        let pid_file_for_thread = pid_file.clone();
        let status = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .unwrap()
                .block_on(async move {
                    let (route, argv) = configured_args(
                        &[
                            "descendant-file".to_string(),
                            pid_file_for_thread.to_string_lossy().to_string(),
                        ],
                        1,
                        1,
                        5,
                    );
                    let permit = route.try_acquire().unwrap();
                    execute(route, argv, Body::empty(), permit).await.status()
                })
        })
        .join()
        .unwrap();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!pid_file.exists(), "timer panic must happen before spawn");
    }

    #[tokio::test]
    async fn request_body_failure_kills_child_and_returns_400() {
        let body = Body::from_stream(futures_util::stream::iter([Err::<Bytes, std::io::Error>(
            std::io::Error::other("request failed"),
        )]));
        let response = run("eof", body, 16, 5).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    async fn assert_process_gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while Path::new("/proc").join(pid.to_string()).exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("process group descendant was killed");
    }

    #[tokio::test]
    async fn request_failure_kills_descendant_process_group() {
        let pid_file = std::env::temp_dir().join(format!(
            "purecas-request-failure-descendant-{}",
            std::process::id()
        ));
        let body = Body::from_stream(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err::<Bytes, std::io::Error>(std::io::Error::other("request failed"))
        }));
        let response = run_args(
            &[
                "descendant-file".to_string(),
                pid_file.to_string_lossy().to_string(),
            ],
            body,
            16,
            5,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let descendant: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        assert_process_gone(descendant).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test]
    async fn timeout_kills_descendant_process_group() {
        let pid_file =
            std::env::temp_dir().join(format!("purecas-timeout-descendant-{}", std::process::id()));
        let response = run_args(
            &[
                "descendant-file".to_string(),
                pid_file.to_string_lossy().to_string(),
            ],
            Body::empty(),
            1,
            1,
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let descendant: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        assert_process_gone(descendant).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test]
    async fn terminal_child_status_never_leaves_redirected_descendants() {
        for (mode, expected) in [
            ("exit-descendant-file", StatusCode::OK),
            ("fail-descendant-file", StatusCode::BAD_GATEWAY),
        ] {
            let pid_file = std::env::temp_dir().join(format!(
                "purecas-terminal-descendant-{}-{mode}",
                std::process::id()
            ));
            let response = run_args(
                &[mode.to_string(), pid_file.to_string_lossy().to_string()],
                Body::empty(),
                1,
                5,
            )
            .await;
            assert_eq!(response.status(), expected);
            let descendant: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
            assert_process_gone(descendant).await;
            let _ = std::fs::remove_file(pid_file);
        }
    }

    #[tokio::test]
    async fn response_drop_kills_process_group_descendant() {
        let response = run("descendant", Body::empty(), 1, 5).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let frame = body
            .frame()
            .await
            .expect("fixture emitted child pid")
            .unwrap();
        let pid_text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        let descendant: u32 = pid_text.trim().parse().unwrap();
        drop(body);

        assert_process_gone(descendant).await;
    }

    #[tokio::test]
    async fn server_shutdown_kills_process_group_descendant() {
        let (route, argv) = configured("descendant", 1, 1, 30);
        let permit = route.try_acquire().unwrap();
        let response = execute(Arc::clone(&route), argv, Body::empty(), permit).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let frame = body
            .frame()
            .await
            .expect("fixture emitted child pid")
            .unwrap();
        let pid_text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        let descendant: u32 = pid_text.trim().parse().unwrap();

        route.cancel();

        assert!(body.collect().await.is_err());
        assert_process_gone(descendant).await;
    }
}
