use super::*;
use crate::PreparedNativeLaunch;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use uuid::Uuid;

const SUPERVISOR_PROTOCOL_VERSION: u32 = 3;
const MAX_CONTROL_FRAME_BYTES: usize = 256 * 1024;
#[cfg(not(test))]
const SUPERVISOR_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SUPERVISOR_IDLE_TIMEOUT: Duration = Duration::from_millis(100);
const REQUIRED_SUPERVISOR_CAPABILITIES: [&str; 5] = [
    "pty.start.generation-fenced.v1",
    "pty.status.v1",
    "pty.write.v1",
    "pty.resize.v1",
    "pty.lifecycle.v1",
];
const SUPERVISOR_LIFECYCLE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SupervisorLifecycleState {
    Ready,
    Stopping,
    Stopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SupervisorShutdownReason {
    IdleTimeout,
    ListenerError,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SupervisorPredecessor {
    supervisor_id: Uuid,
    process_id: u32,
    clean_shutdown: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SupervisorLifecycle {
    schema_version: u32,
    supervisor_id: Uuid,
    process_id: u32,
    protocol_version: u32,
    capabilities: Vec<String>,
    state: SupervisorLifecycleState,
    active_sessions: usize,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    shutdown_reason: Option<SupervisorShutdownReason>,
    predecessor: Option<SupervisorPredecessor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedSupervisorLifecycle {
    lifecycle: SupervisorLifecycle,
    digest: String,
}

impl SupervisorLifecycle {
    fn is_well_formed(&self) -> bool {
        self.schema_version == SUPERVISOR_LIFECYCLE_SCHEMA_VERSION
            && !self.supervisor_id.is_nil()
            && self.process_id > 0
            && self.protocol_version == SUPERVISOR_PROTOCOL_VERSION
            && REQUIRED_SUPERVISOR_CAPABILITIES
                .iter()
                .all(|required| self.capabilities.iter().any(|actual| actual == required))
            && self.active_sessions <= MAX_PROCESS_SESSIONS
            && self.updated_at >= self.started_at
            && match self.state {
                SupervisorLifecycleState::Ready => self.shutdown_reason.is_none(),
                SupervisorLifecycleState::Stopping | SupervisorLifecycleState::Stopped => {
                    self.shutdown_reason.is_some()
                }
            }
            && self.predecessor.as_ref().is_none_or(|predecessor| {
                !predecessor.supervisor_id.is_nil() && predecessor.process_id > 0
            })
    }
}

fn supervisor_capabilities() -> Vec<String> {
    REQUIRED_SUPERVISOR_CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SupervisorLaunch {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    current_dir: PathBuf,
}

impl From<PreparedNativeLaunch> for SupervisorLaunch {
    fn from(launch: PreparedNativeLaunch) -> Self {
        Self {
            program: launch.program,
            args: launch.args,
            env: launch.env,
            current_dir: launch.current_dir,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorCommand {
    Hello {
        min_protocol_version: u32,
        max_protocol_version: u32,
        required_capabilities: Vec<String>,
    },
    Start {
        expected_supervisor_id: Uuid,
        session_id: Uuid,
        launch: SupervisorLaunch,
        initial_stdin: Vec<u8>,
        cols: u16,
        rows: u16,
        max_output_chunk_bytes: usize,
        governance: Box<ProcessSessionGovernance>,
    },
    Status {
        session_id: Uuid,
        supervisor_id: Uuid,
    },
    Write {
        session_id: Uuid,
        supervisor_id: Uuid,
        bytes: Vec<u8>,
    },
    Resize {
        session_id: Uuid,
        supervisor_id: Uuid,
        cols: u16,
        rows: u16,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SupervisorRequest {
    schema_version: u32,
    request_id: Uuid,
    token: String,
    command: SupervisorCommand,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorResult {
    Hello {
        supervisor_id: Uuid,
        protocol_version: u32,
        capabilities: Vec<String>,
    },
    Started {
        supervisor_id: Uuid,
        pid: u32,
    },
    GenerationChanged {
        supervisor_id: Uuid,
    },
    Status {
        live: bool,
        pid: Option<u32>,
    },
    Ack,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SupervisorResponse {
    schema_version: u32,
    request_id: Uuid,
    result: Option<SupervisorResult>,
    error: Option<SupervisorResponseError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SupervisorResponseErrorCode {
    Incompatible,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SupervisorResponseError {
    code: SupervisorResponseErrorCode,
    message: String,
}

struct SupervisedPty {
    supervisor_id: Uuid,
    pid: u32,
    _master: File,
    writer: Mutex<File>,
}

pub(super) struct PtySupervisorStartRequest {
    pub(super) session_id: Uuid,
    pub(super) launch: PreparedNativeLaunch,
    pub(super) initial_stdin: Vec<u8>,
    pub(super) size: ProcessTerminalSize,
    pub(super) max_output_chunk_bytes: usize,
    pub(super) governance: ProcessSessionGovernance,
}

pub(super) fn validate_config(
    config: &ProcessSessionPtySupervisorConfig,
) -> Result<(), ProcessSessionError> {
    if config.executable.as_os_str().is_empty()
        || !config.executable.is_absolute()
        || config.fixed_args.len() > 32
        || config.fixed_args.iter().any(|arg| arg.len() > 4096)
        || config.startup_timeout.is_zero()
        || config.startup_timeout > Duration::from_secs(30)
    {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor command is invalid".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(&config.executable).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor executable must be a regular file, not a symlink".into(),
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor executable is not executable".into(),
        ));
    }
    Ok(())
}

fn socket_path(state_root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    let digest = sha256(state_root.as_os_str().as_bytes());
    socket_root_path().join(format!("{digest}.sock"))
}

fn socket_root_path() -> PathBuf {
    PathBuf::from(format!("/tmp/agent-pty-{}", unsafe { libc::geteuid() }))
}

struct SupervisorSocketCleanup(Option<PathBuf>);

impl SupervisorSocketCleanup {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn cleanup_now(&mut self) -> Result<(), ProcessSessionError> {
        let Some(path) = self.0.as_ref() else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => {
                self.0 = None;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.0 = None;
                Ok(())
            }
            Err(error) => Err(io_error(error)),
        }
    }
}

impl Drop for SupervisorSocketCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn ensure_socket_root() -> Result<(), ProcessSessionError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let root = socket_root_path();
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let metadata = std::fs::symlink_metadata(&root).map_err(io_error)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor socket root must be an owner-only directory".into(),
        ));
    }
    Ok(())
}

fn token_path(state_root: &Path) -> PathBuf {
    state_root.join("process-sessions.supervisor.token")
}

fn startup_lock_path(state_root: &Path) -> PathBuf {
    state_root.join("process-sessions.supervisor.lock")
}

fn lifecycle_path(state_root: &Path) -> PathBuf {
    state_root.join("process-sessions.supervisor-state.json")
}

fn persist_lifecycle(
    state_root: &Path,
    lifecycle: &SupervisorLifecycle,
) -> Result<(), ProcessSessionError> {
    if !lifecycle.is_well_formed() {
        return Err(ProcessSessionError::Indeterminate);
    }
    let lifecycle_bytes = serde_json::to_vec(lifecycle)
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    let persisted = PersistedSupervisorLifecycle {
        lifecycle: lifecycle.clone(),
        digest: sha256(&lifecycle_bytes),
    };
    let path = lifecycle_path(state_root);
    let staging = state_root.join("process-sessions.supervisor-state.json.partial");
    let bytes = serde_json::to_vec_pretty(&persisted)
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    let mut file = open_private_staging_file(&staging)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(staging, path).map_err(io_error)?;
    File::open(state_root)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

fn load_lifecycle(state_root: &Path) -> Result<Option<SupervisorLifecycle>, ProcessSessionError> {
    let bytes = match std::fs::read(lifecycle_path(state_root)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let persisted: PersistedSupervisorLifecycle =
        serde_json::from_slice(&bytes).map_err(|_| ProcessSessionError::Indeterminate)?;
    let lifecycle_bytes =
        serde_json::to_vec(&persisted.lifecycle).map_err(|_| ProcessSessionError::Indeterminate)?;
    if !persisted.lifecycle.is_well_formed() || persisted.digest != sha256(&lifecycle_bytes) {
        return Err(ProcessSessionError::Indeterminate);
    }
    Ok(Some(persisted.lifecycle))
}

fn update_lifecycle(
    state_root: &Path,
    lifecycle: &Arc<Mutex<SupervisorLifecycle>>,
    update: impl FnOnce(&mut SupervisorLifecycle),
) -> Result<(), ProcessSessionError> {
    let mut lifecycle = lifecycle
        .lock()
        .map_err(|_| ProcessSessionError::Io("PTY supervisor lifecycle is poisoned".into()))?;
    update(&mut lifecycle);
    lifecycle.updated_at = Utc::now().max(lifecycle.updated_at);
    persist_lifecycle(state_root, &lifecycle)
}

fn process_is_alive(process_id: u32) -> bool {
    let Ok(process_id) = i32::try_from(process_id) else {
        return false;
    };
    // SAFETY: signal 0 does not mutate the process. It is used only to avoid
    // replacing a predecessor that may still own live terminal descriptors.
    (unsafe { libc::kill(process_id, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub(super) fn ensure_control_token(state_root: &Path) -> Result<(), ProcessSessionError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    ensure_socket_root()?;
    let path = token_path(state_root);
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            let mut random = [0_u8; 32];
            File::open("/dev/urandom")
                .and_then(|mut source| source.read_exact(&mut random))
                .map_err(io_error)?;
            file.write_all(hex::encode(random).as_bytes())
                .map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(io_error)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor token must be an owner-only regular file".into(),
        ));
    }
    let token = std::fs::read_to_string(path).map_err(io_error)?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor token is malformed".into(),
        ));
    }
    Ok(())
}

fn read_control_token(state_root: &Path) -> Result<String, ProcessSessionError> {
    ensure_control_token(state_root)?;
    std::fs::read_to_string(token_path(state_root)).map_err(io_error)
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, ProcessSessionError> {
    let mut frame = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.map_err(io_error)?;
        if read == 0 {
            return Err(ProcessSessionError::Io(
                "PTY supervisor closed a control frame".into(),
            ));
        }
        if let Some(newline) = buffer[..read].iter().position(|byte| *byte == b'\n') {
            frame.extend_from_slice(&buffer[..newline]);
            break;
        }
        frame.extend_from_slice(&buffer[..read]);
        if frame.len() > MAX_CONTROL_FRAME_BYTES {
            return Err(ProcessSessionError::InvalidRequest(
                "PTY supervisor control frame is too large".into(),
            ));
        }
    }
    if frame.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ProcessSessionError::InvalidRequest(
            "PTY supervisor control frame is too large".into(),
        ));
    }
    Ok(frame)
}

async fn request(
    state_root: &Path,
    command: SupervisorCommand,
) -> Result<SupervisorResult, ProcessSessionError> {
    let request_id = Uuid::now_v7();
    let request = SupervisorRequest {
        schema_version: SUPERVISOR_PROTOCOL_VERSION,
        request_id,
        token: read_control_token(state_root)?,
        command,
    };
    let mut bytes =
        serde_json::to_vec(&request).map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    if bytes.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ProcessSessionError::InvalidRequest(
            "PTY supervisor request is too large".into(),
        ));
    }
    bytes.push(b'\n');
    let mut stream = UnixStream::connect(socket_path(state_root))
        .await
        .map_err(io_error)?;
    stream.write_all(&bytes).await.map_err(io_error)?;
    stream.flush().await.map_err(io_error)?;
    let frame = read_frame(&mut stream).await?;
    let response: SupervisorResponse =
        serde_json::from_slice(&frame).map_err(|_| ProcessSessionError::Indeterminate)?;
    if response.schema_version != SUPERVISOR_PROTOCOL_VERSION
        || response.request_id != request_id
        || response.result.is_some() == response.error.is_some()
    {
        return Err(ProcessSessionError::Indeterminate);
    }
    response.result.ok_or(match response.error {
        Some(SupervisorResponseError {
            code: SupervisorResponseErrorCode::Incompatible,
            message,
        }) => ProcessSessionError::InvalidConfiguration(message),
        Some(SupervisorResponseError { message, .. }) => ProcessSessionError::Io(message),
        None => ProcessSessionError::Indeterminate,
    })
}

fn incompatible_supervisor() -> ProcessSessionError {
    ProcessSessionError::InvalidConfiguration(
        "PTY supervisor protocol or capability handshake is incompatible".into(),
    )
}

async fn ping(state_root: &Path) -> Result<Uuid, ProcessSessionError> {
    let result = request(
        state_root,
        SupervisorCommand::Hello {
            min_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            required_capabilities: supervisor_capabilities(),
        },
    )
    .await
    .map_err(|error| match error {
        ProcessSessionError::Indeterminate => incompatible_supervisor(),
        error => error,
    })?;
    match result {
        SupervisorResult::Hello {
            supervisor_id,
            protocol_version,
            capabilities,
        } if !supervisor_id.is_nil()
            && protocol_version == SUPERVISOR_PROTOCOL_VERSION
            && REQUIRED_SUPERVISOR_CAPABILITIES
                .iter()
                .all(|required| capabilities.iter().any(|actual| actual == required)) =>
        {
            Ok(supervisor_id)
        }
        _ => Err(incompatible_supervisor()),
    }
}

fn is_incompatible(error: &ProcessSessionError) -> bool {
    matches!(error, ProcessSessionError::InvalidConfiguration(message) if message.contains("handshake is incompatible"))
}

pub(super) async fn ensure_running(
    state_root: &Path,
    config: &ProcessSessionPtySupervisorConfig,
) -> Result<Uuid, ProcessSessionError> {
    match ping(state_root).await {
        Ok(supervisor_id) => return Ok(supervisor_id),
        Err(error) if is_incompatible(&error) => return Err(error),
        Err(_) => {}
    }
    let startup_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(startup_lock_path(state_root))
        .map_err(io_error)?;
    lock_exclusive(&startup_lock)?;
    match ping(state_root).await {
        Ok(supervisor_id) => {
            unlock(&startup_lock)?;
            return Ok(supervisor_id);
        }
        Err(error) if is_incompatible(&error) => {
            unlock(&startup_lock)?;
            return Err(error);
        }
        Err(_) => {}
    }
    match std::fs::remove_file(socket_path(state_root)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            unlock(&startup_lock)?;
            return Err(io_error(error));
        }
    }
    let mut command = Command::new(&config.executable);
    command
        .args(&config.fixed_args)
        .arg("--state-root")
        .arg(state_root)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .process_group(0);
    let mut child = command.spawn().map_err(io_error)?;
    let deadline = tokio::time::Instant::now() + config.startup_timeout;
    let result = loop {
        if let Ok(supervisor_id) = ping(state_root).await {
            break Ok(supervisor_id);
        }
        if child.try_wait().map_err(io_error)?.is_some() {
            break Err(ProcessSessionError::Io(
                "PTY supervisor exited during startup".into(),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(ProcessSessionError::Io(
                "PTY supervisor did not become ready before its deadline".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    unlock(&startup_lock)?;
    result
}

pub(super) async fn start(
    state_root: &Path,
    config: &ProcessSessionPtySupervisorConfig,
    start: PtySupervisorStartRequest,
) -> Result<(Uuid, u32), ProcessSessionError> {
    let launch = SupervisorLaunch::from(start.launch);
    let mut expected_supervisor = ensure_running(state_root, config).await?;
    for attempt in 0..2 {
        match request(
            state_root,
            SupervisorCommand::Start {
                expected_supervisor_id: expected_supervisor,
                session_id: start.session_id,
                launch: launch.clone(),
                initial_stdin: start.initial_stdin.clone(),
                cols: start.size.cols,
                rows: start.size.rows,
                max_output_chunk_bytes: start.max_output_chunk_bytes,
                governance: Box::new(start.governance.clone()),
            },
        )
        .await?
        {
            SupervisorResult::Started { supervisor_id, pid }
                if supervisor_id == expected_supervisor && pid > 0 =>
            {
                return Ok((supervisor_id, pid));
            }
            SupervisorResult::GenerationChanged { supervisor_id }
                if !supervisor_id.is_nil() && supervisor_id != expected_supervisor =>
            {
                // The rejecting generation proved that no process was
                // started. Re-handshake once, then retry the exact request.
                if attempt > 0 {
                    return Err(ProcessSessionError::Io(
                        "PTY supervisor generation changed repeatedly before start".into(),
                    ));
                }
                expected_supervisor = ensure_running(state_root, config).await?;
            }
            _ => {
                return Err(ProcessSessionError::Indeterminate);
            }
        }
    }
    unreachable!("bounded PTY supervisor start loop always returns")
}

pub(super) async fn status(
    state_root: &Path,
    session_id: Uuid,
    supervisor_id: Uuid,
) -> Result<Option<u32>, ProcessSessionError> {
    match request(
        state_root,
        SupervisorCommand::Status {
            session_id,
            supervisor_id,
        },
    )
    .await?
    {
        SupervisorResult::Status { live: true, pid } => {
            pid.ok_or(ProcessSessionError::Indeterminate).map(Some)
        }
        SupervisorResult::Status {
            live: false,
            pid: None,
        } => Ok(None),
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

pub(super) async fn write(
    state_root: &Path,
    session_id: Uuid,
    supervisor_id: Uuid,
    bytes: Vec<u8>,
) -> Result<(), ProcessSessionError> {
    match request(
        state_root,
        SupervisorCommand::Write {
            session_id,
            supervisor_id,
            bytes,
        },
    )
    .await?
    {
        SupervisorResult::Ack => Ok(()),
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

pub(super) async fn resize(
    state_root: &Path,
    session_id: Uuid,
    supervisor_id: Uuid,
    cols: u16,
    rows: u16,
) -> Result<(), ProcessSessionError> {
    match request(
        state_root,
        SupervisorCommand::Resize {
            session_id,
            supervisor_id,
            cols,
            rows,
        },
    )
    .await?
    {
        SupervisorResult::Ack => Ok(()),
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn supervisor_response(
    request_id: Uuid,
    result: Result<SupervisorResult, ProcessSessionError>,
) -> SupervisorResponse {
    match result {
        Ok(result) => SupervisorResponse {
            schema_version: SUPERVISOR_PROTOCOL_VERSION,
            request_id,
            result: Some(result),
            error: None,
        },
        Err(error) => SupervisorResponse {
            schema_version: SUPERVISOR_PROTOCOL_VERSION,
            request_id,
            result: None,
            error: Some(SupervisorResponseError {
                code: if is_incompatible(&error) {
                    SupervisorResponseErrorCode::Incompatible
                } else {
                    SupervisorResponseErrorCode::Rejected
                },
                message: error.to_string(),
            }),
        },
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    state_root: &Path,
    token: &str,
    supervisor_id: Uuid,
    sessions: &Arc<Mutex<HashMap<Uuid, Arc<SupervisedPty>>>>,
    lifecycle: &Arc<Mutex<SupervisorLifecycle>>,
) -> Result<(), ProcessSessionError> {
    let frame = read_frame(&mut stream).await?;
    let request: SupervisorRequest = serde_json::from_slice(&frame)
        .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
    let request_id = request.request_id;
    let result = if request.schema_version != SUPERVISOR_PROTOCOL_VERSION
        || request.request_id.is_nil()
        || request.token != token
    {
        Err(ProcessSessionError::AccessDenied)
    } else {
        handle_command(
            state_root,
            supervisor_id,
            sessions,
            lifecycle,
            request.command,
        )
        .await
    };
    let mut bytes = serde_json::to_vec(&supervisor_response(request_id, result))
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(io_error)?;
    stream.flush().await.map_err(io_error)
}

async fn handle_command(
    state_root: &Path,
    supervisor_id: Uuid,
    sessions: &Arc<Mutex<HashMap<Uuid, Arc<SupervisedPty>>>>,
    lifecycle: &Arc<Mutex<SupervisorLifecycle>>,
    command: SupervisorCommand,
) -> Result<SupervisorResult, ProcessSessionError> {
    match command {
        SupervisorCommand::Hello {
            min_protocol_version,
            max_protocol_version,
            required_capabilities,
        } => {
            let capabilities = supervisor_capabilities();
            if min_protocol_version > SUPERVISOR_PROTOCOL_VERSION
                || max_protocol_version < SUPERVISOR_PROTOCOL_VERSION
                || required_capabilities
                    .iter()
                    .any(|required| !capabilities.contains(required))
            {
                return Err(incompatible_supervisor());
            }
            Ok(SupervisorResult::Hello {
                supervisor_id,
                protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                capabilities,
            })
        }
        SupervisorCommand::Start {
            expected_supervisor_id,
            session_id,
            launch,
            initial_stdin,
            cols,
            rows,
            max_output_chunk_bytes,
            governance,
        } => {
            if expected_supervisor_id != supervisor_id {
                return Ok(SupervisorResult::GenerationChanged { supervisor_id });
            }
            let pid = start_session(
                state_root,
                supervisor_id,
                sessions,
                lifecycle,
                session_id,
                launch,
                initial_stdin,
                ProcessTerminalSize { cols, rows },
                max_output_chunk_bytes,
                *governance,
            )
            .await?;
            Ok(SupervisorResult::Started { supervisor_id, pid })
        }
        SupervisorCommand::Status {
            session_id,
            supervisor_id: expected,
        } => {
            let live = sessions
                .lock()
                .map_err(|_| ProcessSessionError::Io("PTY supervisor registry is poisoned".into()))?
                .get(&session_id)
                .filter(|session| session.supervisor_id == expected && expected == supervisor_id)
                .map(|session| session.pid);
            Ok(SupervisorResult::Status {
                live: live.is_some(),
                pid: live,
            })
        }
        SupervisorCommand::Write {
            session_id,
            supervisor_id: expected,
            bytes,
        } => {
            if bytes.is_empty() || bytes.len() > MAX_STDIN_BYTES {
                return Err(ProcessSessionError::InvalidRequest(
                    "PTY stdin write must contain 1 to 65536 bytes".into(),
                ));
            }
            let session = sessions
                .lock()
                .map_err(|_| ProcessSessionError::Io("PTY supervisor registry is poisoned".into()))?
                .get(&session_id)
                .filter(|session| session.supervisor_id == expected && expected == supervisor_id)
                .cloned()
                .ok_or(ProcessSessionError::NotFound)?;
            let mut writer = session
                .writer
                .lock()
                .map_err(|_| ProcessSessionError::Io("PTY supervisor writer is poisoned".into()))?;
            writer.write_all(&bytes).map_err(io_error)?;
            writer.flush().map_err(io_error)?;
            Ok(SupervisorResult::Ack)
        }
        SupervisorCommand::Resize {
            session_id,
            supervisor_id: expected,
            cols,
            rows,
        } => {
            if cols == 0 || rows == 0 || cols > 2_000 || rows > 2_000 {
                return Err(ProcessSessionError::InvalidRequest(
                    "PTY dimensions must be between 1 and 2000 cells".into(),
                ));
            }
            let session = sessions
                .lock()
                .map_err(|_| ProcessSessionError::Io("PTY supervisor registry is poisoned".into()))?
                .get(&session_id)
                .filter(|session| session.supervisor_id == expected && expected == supervisor_id)
                .cloned()
                .ok_or(ProcessSessionError::NotFound)?;
            use std::os::fd::AsRawFd;
            let mut size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: the supervisor owns this live PTY master descriptor and
            // validates both dimensions before issuing TIOCSWINSZ.
            if unsafe {
                libc::ioctl(
                    session._master.as_raw_fd(),
                    libc::TIOCSWINSZ as _,
                    &mut size,
                )
            } < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            persist_terminal_marker(
                &state_root
                    .join("process-sessions")
                    .join(session_id.to_string()),
                &ProcessTerminalMarker {
                    schema_version: 2,
                    session_id,
                    cols,
                    rows,
                    supervisor_id: Some(supervisor_id),
                },
            )?;
            Ok(SupervisorResult::Ack)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_session(
    state_root: &Path,
    supervisor_id: Uuid,
    sessions: &Arc<Mutex<HashMap<Uuid, Arc<SupervisedPty>>>>,
    lifecycle: &Arc<Mutex<SupervisorLifecycle>>,
    session_id: Uuid,
    launch: SupervisorLaunch,
    initial_stdin: Vec<u8>,
    size: ProcessTerminalSize,
    max_output_chunk_bytes: usize,
    governance: ProcessSessionGovernance,
) -> Result<u32, ProcessSessionError> {
    if session_id.is_nil()
        || size.cols == 0
        || size.rows == 0
        || size.cols > 2_000
        || size.rows > 2_000
        || initial_stdin.len() > MAX_STDIN_BYTES
    {
        return Err(ProcessSessionError::InvalidRequest(
            "PTY supervisor start request is invalid".into(),
        ));
    }
    let capabilities = resolve_resource_capabilities(&governance.resource_backend)?;
    validate_governance(&governance, max_output_chunk_bytes, capabilities)?;
    let resource_backend = ProcessSessionResourceBackend::open(&governance.resource_backend)?;
    let session_dir = state_root
        .join("process-sessions")
        .join(session_id.to_string());
    let session_dir = std::fs::canonicalize(session_dir).map_err(io_error)?;
    if !session_dir.starts_with(state_root.join("process-sessions")) {
        return Err(ProcessSessionError::AccessDenied);
    }
    let mut manifest = load_manifest(&session_dir)?;
    if manifest.session_id != session_id
        || manifest.state != ProcessSessionState::Starting
        || manifest.resource_phase != ProcessSessionResourcePhase::Unprepared
        || manifest.governance_digest != governance_digest(&governance, capabilities)
        || manifest.resource_identity
            != ProcessSessionResourceIdentity::for_backend(&resource_backend, session_id)
    {
        return Err(ProcessSessionError::Conflict);
    }
    if sessions
        .lock()
        .map_err(|_| ProcessSessionError::Io("PTY supervisor registry is poisoned".into()))?
        .contains_key(&session_id)
    {
        return Err(ProcessSessionError::Conflict);
    }

    let (master, slave) = open_pty(size)?;
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .env_clear()
        .envs(launch.env.iter().cloned())
        .current_dir(&launch.current_dir)
        .stdin(Stdio::from(slave.try_clone().map_err(io_error)?))
        .stdout(Stdio::from(slave.try_clone().map_err(io_error)?))
        .stderr(Stdio::from(slave.try_clone().map_err(io_error)?))
        .kill_on_drop(false);
    install_controlling_terminal(&mut command, &slave)?;
    install_identity_lease(&mut command, &session_dir.join("identity.lock"))?;
    install_process_resource_limits(
        &mut command,
        governance.max_output_bytes_per_stream,
        governance.max_cpu_seconds,
        governance.max_memory_bytes,
    )?;
    let mut prepared_linux_group = match (&resource_backend, &manifest.resource_identity) {
        (
            ProcessSessionResourceBackend::LinuxCgroupV2 { root },
            ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
        ) => Some(
            prepare_linux_cgroup_v2_root(
                root,
                group_name,
                governance.max_memory_bytes,
                governance.max_processes,
            )
            .map_err(|error| ProcessSessionError::InvalidConfiguration(error.to_string()))?,
        ),
        (ProcessSessionResourceBackend::UnixRlimit, ProcessSessionResourceIdentity::UnixRlimit) => {
            None
        }
        _ => return Err(ProcessSessionError::Indeterminate),
    };
    manifest.resource_phase = ProcessSessionResourcePhase::Prepared;
    manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
    manifest.last_operation = if prepared_linux_group.is_some() {
        "resource_prepared".into()
    } else {
        "launch_prepared".into()
    };
    manifest.updated_at = Utc::now().max(manifest.updated_at);
    persist_manifest(&session_dir, &manifest)?;
    if let Some(group) = &prepared_linux_group {
        install_linux_cgroup_membership_group(&mut command, group)
            .map_err(|error| ProcessSessionError::InvalidConfiguration(error.to_string()))?;
    }
    let spawn_guard = PROCESS_SESSION_SPAWN_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| ProcessSessionError::Io("process-session spawn lock is poisoned".into()))?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(spawn_guard);
            drop(prepared_linux_group.take());
            let reason = error.to_string();
            finalize_prepared_start_failure(&session_dir, &manifest)?;
            let terminal = load_manifest(&session_dir)?;
            let _ = cleanup_terminal_resource_identity(&session_dir, &terminal, &resource_backend);
            return Err(ProcessSessionError::StartFailed { session_id, reason });
        }
    };
    drop(spawn_guard);
    drop(prepared_linux_group);
    drop(slave);
    let pid = child
        .id()
        .ok_or_else(|| ProcessSessionError::Io("spawned PTY process has no pid".into()))?;
    manifest.state = ProcessSessionState::Running;
    manifest.resource_phase = ProcessSessionResourcePhase::Active;
    manifest.pid = Some(pid);
    manifest.process_group_id = i32::try_from(pid).ok();
    manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
    manifest.last_operation = "started".into();
    manifest.updated_at = Utc::now();
    persist_terminal_marker(
        &session_dir,
        &ProcessTerminalMarker {
            schema_version: 2,
            session_id,
            cols: size.cols,
            rows: size.rows,
            supervisor_id: Some(supervisor_id),
        },
    )?;
    persist_manifest(&session_dir, &manifest)?;

    let mut reader = master.try_clone().map_err(io_error)?;
    let writer = master.try_clone().map_err(io_error)?;
    let live = Arc::new(SupervisedPty {
        supervisor_id,
        pid,
        _master: master,
        writer: Mutex::new(writer),
    });
    sessions
        .lock()
        .map_err(|_| ProcessSessionError::Io("PTY supervisor registry is poisoned".into()))?
        .insert(session_id, live.clone());
    if let Err(error) = update_lifecycle(state_root, lifecycle, |current| {
        current.active_sessions = sessions.lock().map(|sessions| sessions.len()).unwrap_or(0);
    }) {
        let _ = signal_group(i32::try_from(pid).unwrap_or(i32::MAX), libc::SIGKILL);
        if let Ok(mut sessions) = sessions.lock() {
            sessions.remove(&session_id);
        }
        let _ = child.wait().await;
        return Err(error);
    }
    if !initial_stdin.is_empty() {
        let mut writer = live
            .writer
            .lock()
            .map_err(|_| ProcessSessionError::Io("PTY supervisor writer is poisoned".into()))?;
        writer.write_all(&initial_stdin).map_err(io_error)?;
        writer.flush().map_err(io_error)?;
    }

    let output_path = session_dir.join("stdout.log");
    let output_limit = governance.max_output_bytes_per_stream;
    let process_group_id = i32::try_from(pid).map_err(|_| ProcessSessionError::Indeterminate)?;
    tokio::task::spawn_blocking(move || {
        let Ok(mut output) = open_append(&output_path) else {
            let _ = signal_group(process_group_id, libc::SIGKILL);
            return;
        };
        let mut written = output
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = output_limit.saturating_sub(written);
                    let accepted = read.min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    if accepted > 0
                        && (output.write_all(&buffer[..accepted]).is_err()
                            || output.flush().is_err())
                    {
                        let _ = signal_group(process_group_id, libc::SIGKILL);
                        break;
                    }
                    written = written.saturating_add(u64::try_from(accepted).unwrap_or(u64::MAX));
                    if accepted < read || written >= output_limit {
                        let _ = signal_group(process_group_id, libc::SIGTERM);
                        std::thread::sleep(CLOSE_GRACE);
                        let _ = signal_group(process_group_id, libc::SIGKILL);
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => {
                    let _ = signal_group(process_group_id, libc::SIGKILL);
                    break;
                }
            }
        }
    });

    let watched_dir = session_dir.clone();
    let watched_manifest = manifest.clone();
    let watched_resource_backend = resource_backend.clone();
    let watched_sessions = sessions.clone();
    let watched_lifecycle = lifecycle.clone();
    let watched_state_root = state_root.to_path_buf();
    tokio::spawn(async move {
        let status = child.wait().await;
        let _ =
            terminate_resource_identity(&watched_dir, &watched_manifest, &watched_resource_backend)
                .await;
        let _ =
            finalize_exited_manifest(&watched_dir, status.ok().and_then(|status| status.code()));
        if let Ok(terminal) = load_manifest(&watched_dir) {
            let _ = cleanup_terminal_resource_identity(
                &watched_dir,
                &terminal,
                &watched_resource_backend,
            );
        }
        if let Ok(mut sessions) = watched_sessions.lock() {
            sessions.remove(&session_id);
        }
        let _ = update_lifecycle(&watched_state_root, &watched_lifecycle, |current| {
            current.active_sessions = watched_sessions
                .lock()
                .map(|sessions| sessions.len())
                .unwrap_or(0);
        });
    });
    let governed_dir = session_dir;
    tokio::spawn(async move {
        supervise_process_governance(governed_dir, resource_backend).await;
    });
    Ok(pid)
}

pub async fn run_process_session_pty_supervisor(
    state_root: PathBuf,
) -> Result<(), ProcessSessionError> {
    use std::os::unix::fs::PermissionsExt;

    let state_root = std::fs::canonicalize(state_root).map_err(io_error)?;
    if !state_root.is_dir() {
        return Err(ProcessSessionError::InvalidConfiguration(
            "PTY supervisor state root must be a directory".into(),
        ));
    }
    ensure_control_token(&state_root)?;
    let token = read_control_token(&state_root)?;
    let socket = socket_path(&state_root);
    if UnixStream::connect(&socket).await.is_ok() {
        return Err(ProcessSessionError::Conflict);
    }
    let predecessor = load_lifecycle(&state_root)?;
    if predecessor.as_ref().is_some_and(|previous| {
        previous.state != SupervisorLifecycleState::Stopped && process_is_alive(previous.process_id)
    }) {
        return Err(ProcessSessionError::Conflict);
    }
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    let listener = UnixListener::bind(&socket).map_err(io_error)?;
    let mut socket_cleanup_guard = SupervisorSocketCleanup::new(socket.clone());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    let supervisor_id = Uuid::now_v7();
    let now = Utc::now();
    let lifecycle = Arc::new(Mutex::new(SupervisorLifecycle {
        schema_version: SUPERVISOR_LIFECYCLE_SCHEMA_VERSION,
        supervisor_id,
        process_id: std::process::id(),
        protocol_version: SUPERVISOR_PROTOCOL_VERSION,
        capabilities: supervisor_capabilities(),
        state: SupervisorLifecycleState::Ready,
        active_sessions: 0,
        started_at: now,
        updated_at: now,
        shutdown_reason: None,
        predecessor: predecessor.map(|previous| SupervisorPredecessor {
            supervisor_id: previous.supervisor_id,
            process_id: previous.process_id,
            clean_shutdown: previous.state == SupervisorLifecycleState::Stopped,
        }),
    }));
    {
        let lifecycle = lifecycle
            .lock()
            .map_err(|_| ProcessSessionError::Io("PTY supervisor lifecycle is poisoned".into()))?;
        persist_lifecycle(&state_root, &lifecycle)?;
    }
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let exit = loop {
        match tokio::time::timeout(SUPERVISOR_IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let _ = handle_connection(
                    stream,
                    &state_root,
                    &token,
                    supervisor_id,
                    &sessions,
                    &lifecycle,
                )
                .await;
            }
            Ok(Err(error)) => {
                break Err((SupervisorShutdownReason::ListenerError, io_error(error)));
            }
            Err(_)
                if sessions
                    .lock()
                    .map_err(|_| {
                        ProcessSessionError::Io("PTY supervisor registry is poisoned".into())
                    })?
                    .is_empty() =>
            {
                break Ok(SupervisorShutdownReason::IdleTimeout);
            }
            Err(_) => {}
        }
    };
    let reason = match &exit {
        Ok(reason) => *reason,
        Err((reason, _)) => *reason,
    };
    update_lifecycle(&state_root, &lifecycle, |current| {
        current.state = SupervisorLifecycleState::Stopping;
        current.active_sessions = 0;
        current.shutdown_reason = Some(reason);
    })?;
    drop(listener);
    let socket_cleanup = socket_cleanup_guard.cleanup_now();
    update_lifecycle(&state_root, &lifecycle, |current| {
        current.state = SupervisorLifecycleState::Stopped;
        current.shutdown_reason = Some(reason);
    })?;
    socket_cleanup?;
    match exit {
        Ok(_) => Ok(()),
        Err((_, error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production break this catches is a short-lived test or embedding
    /// deleting its state root before the idle supervisor writes `Stopped`.
    /// The lifecycle write may fail, but the global Unix socket must not be
    /// left behind as development garbage.
    #[tokio::test]
    async fn supervisor_removes_its_socket_when_the_state_root_disappears() {
        let state = tempfile::tempdir().unwrap();
        ensure_control_token(state.path()).unwrap();
        let state_root = std::fs::canonicalize(state.path()).unwrap();
        let socket = socket_path(&state_root);
        let task = tokio::spawn(run_process_session_pty_supervisor(state_root.clone()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ping_result = ping(&state_root).await;
            if ping_result.is_ok() {
                break;
            }
            if task.is_finished() {
                panic!("supervisor exited during startup: {:?}", task.await);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervisor did not become ready: {ping_result:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        state.close().unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(7), task)
            .await
            .expect("idle supervisor did not stop")
            .unwrap();
        assert!(
            !socket.exists(),
            "supervisor left a stale Unix socket after its state root disappeared"
        );
    }

    #[tokio::test]
    async fn completed_socket_cleanup_cannot_unlink_a_successor_generation() {
        let state = tempfile::tempdir().unwrap();
        let state_root = std::fs::canonicalize(state.path()).unwrap();
        ensure_control_token(&state_root).unwrap();
        let socket = socket_path(&state_root);
        let listener = UnixListener::bind(&socket).unwrap();
        let mut predecessor_cleanup = SupervisorSocketCleanup::new(socket.clone());
        drop(listener);
        predecessor_cleanup.cleanup_now().unwrap();

        let successor = UnixListener::bind(&socket).unwrap();
        drop(predecessor_cleanup);
        assert!(
            socket.exists(),
            "the predecessor cleanup removed the successor generation socket"
        );
        drop(successor);
        std::fs::remove_file(socket).unwrap();
    }

    #[tokio::test]
    async fn stale_start_generation_is_rejected_before_process_side_effects() {
        let state = tempfile::tempdir().unwrap();
        let current = Uuid::now_v7();
        let stale = Uuid::now_v7();
        let now = Utc::now();
        let lifecycle = Arc::new(Mutex::new(SupervisorLifecycle {
            schema_version: SUPERVISOR_LIFECYCLE_SCHEMA_VERSION,
            supervisor_id: current,
            process_id: std::process::id(),
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            capabilities: supervisor_capabilities(),
            state: SupervisorLifecycleState::Ready,
            active_sessions: 0,
            started_at: now,
            updated_at: now,
            shutdown_reason: None,
            predecessor: None,
        }));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let result = handle_command(
            state.path(),
            current,
            &sessions,
            &lifecycle,
            SupervisorCommand::Start {
                expected_supervisor_id: stale,
                session_id: Uuid::now_v7(),
                launch: SupervisorLaunch {
                    program: PathBuf::from("/must-not-run"),
                    args: Vec::new(),
                    env: Vec::new(),
                    current_dir: state.path().to_path_buf(),
                },
                initial_stdin: Vec::new(),
                cols: 80,
                rows: 24,
                max_output_chunk_bytes: 1024,
                governance: Box::new(ProcessSessionGovernance::default()),
            },
        )
        .await
        .expect("generation mismatch is a retry-safe protocol result");

        assert!(matches!(
            result,
            SupervisorResult::GenerationChanged { supervisor_id }
                if supervisor_id == current
        ));
        assert!(sessions.lock().unwrap().is_empty());
        assert!(!state.path().join("process-sessions").exists());
    }

    /// The production break this catches is accepting a stale supervisor that
    /// can answer the old ping but cannot prove support for the operations the
    /// Runtime will issue after reattachment.
    #[tokio::test]
    async fn handshake_rejects_a_supervisor_that_omits_required_capabilities() {
        let state = tempfile::tempdir().unwrap();
        ensure_control_token(state.path()).unwrap();
        let socket = socket_path(state.path());
        let _cleanup = SupervisorSocketCleanup::new(socket.clone());
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut stream).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            let mut response = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "request_id": request["request_id"],
                "result": {
                    "type": "pong",
                    "supervisor_id": Uuid::now_v7(),
                },
                "error": null,
            }))
            .unwrap();
            response.push(b'\n');
            stream.write_all(&response).await.unwrap();
        });

        let result = ping(state.path()).await;
        server.await.unwrap();
        assert!(matches!(
            result,
            Err(ProcessSessionError::InvalidConfiguration(_))
        ));
    }

    #[tokio::test]
    async fn incompatible_predecessor_is_not_unlinked_or_replaced() {
        let state = tempfile::tempdir().unwrap();
        ensure_control_token(state.path()).unwrap();
        let socket = socket_path(state.path());
        let _cleanup = SupervisorSocketCleanup::new(socket.clone());
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame = read_frame(&mut stream).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&frame).unwrap();
            let mut response = serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "request_id": request["request_id"],
                "result": null,
                "error": "old supervisor lacks generation-fenced start"
            }))
            .unwrap();
            response.push(b'\n');
            stream.write_all(&response).await.unwrap();
        });
        let config = ProcessSessionPtySupervisorConfig {
            executable: PathBuf::from("/bin/sh"),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(1),
        };

        let result = ensure_running(state.path(), &config).await;
        server.await.unwrap();
        assert!(matches!(
            result,
            Err(ProcessSessionError::InvalidConfiguration(_))
        ));
        assert!(
            socket.exists(),
            "an incompatible live predecessor must not be unlinked"
        );
    }

    #[tokio::test]
    async fn client_rehandshakes_once_and_starts_only_under_the_current_generation() {
        let state = tempfile::tempdir().unwrap();
        let state_root = std::fs::canonicalize(state.path()).unwrap();
        ensure_control_token(&state_root).unwrap();
        let socket = socket_path(&state_root);
        let _cleanup = SupervisorSocketCleanup::new(socket.clone());
        let listener = UnixListener::bind(&socket).unwrap();
        let predecessor = Uuid::now_v7();
        let successor = Uuid::now_v7();
        let server = tokio::spawn(async move {
            let mut accepted_starts = 0;
            for step in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let frame = read_frame(&mut stream).await.unwrap();
                let request: SupervisorRequest = serde_json::from_slice(&frame).unwrap();
                let result = match (step, request.command) {
                    (0, SupervisorCommand::Hello { .. }) => SupervisorResult::Hello {
                        supervisor_id: predecessor,
                        protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                        capabilities: supervisor_capabilities(),
                    },
                    (
                        1,
                        SupervisorCommand::Start {
                            expected_supervisor_id,
                            ..
                        },
                    ) => {
                        assert_eq!(expected_supervisor_id, predecessor);
                        SupervisorResult::GenerationChanged {
                            supervisor_id: successor,
                        }
                    }
                    (2, SupervisorCommand::Hello { .. }) => SupervisorResult::Hello {
                        supervisor_id: successor,
                        protocol_version: SUPERVISOR_PROTOCOL_VERSION,
                        capabilities: supervisor_capabilities(),
                    },
                    (
                        3,
                        SupervisorCommand::Start {
                            expected_supervisor_id,
                            ..
                        },
                    ) => {
                        assert_eq!(expected_supervisor_id, successor);
                        accepted_starts += 1;
                        SupervisorResult::Started {
                            supervisor_id: successor,
                            pid: 123,
                        }
                    }
                    _ => panic!("unexpected supervisor exchange {step}"),
                };
                let mut bytes = serde_json::to_vec(&SupervisorResponse {
                    schema_version: SUPERVISOR_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    result: Some(result),
                    error: None,
                })
                .unwrap();
                bytes.push(b'\n');
                stream.write_all(&bytes).await.unwrap();
            }
            accepted_starts
        });
        let config = ProcessSessionPtySupervisorConfig {
            executable: PathBuf::from("/bin/sh"),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(1),
        };
        let result = start(
            &state_root,
            &config,
            PtySupervisorStartRequest {
                session_id: Uuid::now_v7(),
                launch: PreparedNativeLaunch {
                    program: PathBuf::from("/must-not-run-in-the-fake-server"),
                    args: Vec::new(),
                    env: Vec::new(),
                    current_dir: state_root.clone(),
                    stdin_json: serde_json::Value::Null,
                },
                initial_stdin: Vec::new(),
                size: ProcessTerminalSize { cols: 80, rows: 24 },
                max_output_chunk_bytes: 1024,
                governance: ProcessSessionGovernance::default(),
            },
        )
        .await
        .unwrap();

        assert_eq!(result, (successor, 123));
        assert_eq!(server.await.unwrap(), 1);
    }
}
