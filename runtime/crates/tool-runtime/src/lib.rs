#[cfg(target_os = "macos")]
mod seatbelt;

use agent_protocol::{SandboxClass, ToolExecutionRequest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAccess {
    ReadOnly,
    /// Writes are permitted, but only beneath the canonical Workspace root and
    /// only because containment enforces that boundary.
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerToolDefinition {
    pub image: String,
    pub entrypoint: Vec<String>,
    pub workspace_access: WorkspaceAccess,
    pub memory_bytes: u64,
    pub cpu_millis: u32,
    pub pids_limit: u32,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedNativeToolDefinition {
    pub trusted_root: PathBuf,
    pub executable: PathBuf,
    pub fixed_args: Vec<String>,
    pub workspace_access: WorkspaceAccess,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub workspace_root: PathBuf,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
    pub requested_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedContainerLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin_json: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedNativeLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub current_dir: PathBuf,
    pub stdin_json: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolExecutionResult {
    pub content: Value,
    pub is_error: bool,
    pub exit_code: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolExecutionError {
    #[error("invalid container tool definition: {0}")]
    InvalidDefinition(String),
    #[error("tool request requires a different sandbox provider")]
    WrongSandbox,
    #[error("invalid tool execution context: {0}")]
    InvalidContext(String),
    #[error("tool containment cannot be established: {0}")]
    ContainmentUnavailable(String),
    #[error("container engine failed: {0}")]
    Engine(String),
    #[error("tool execution timed out")]
    TimedOut,
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("tool output exceeded its configured limit")]
    OutputLimitExceeded,
    #[error("tool container exited with code {exit_code}: {stderr}")]
    ProcessFailed { exit_code: i32, stderr: String },
    #[error("tool container returned invalid JSON: {0}")]
    InvalidOutput(String),
    #[error("tool container result does not match the requested tool call")]
    OutputBindingMismatch,
    #[error("trusted native tool executable changed after registration")]
    ExecutableChanged,
}

pub trait ToolExecutor: Send + Sync {
    fn implementation_digest(&self) -> &str;

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>;
}

#[derive(Clone, Debug)]
pub struct RestrictedContainerExecutor {
    engine: String,
    definition: ContainerToolDefinition,
    implementation_digest: String,
}

impl RestrictedContainerExecutor {
    pub fn new(
        engine: impl Into<String>,
        definition: ContainerToolDefinition,
    ) -> Result<Self, ToolExecutionError> {
        let engine = engine.into();
        validate_definition(&engine, &definition)?;
        let implementation_digest = digest_serializable(&json!({
            "image": definition.image,
            "entrypoint": definition.entrypoint,
            "workspace_access": "read_only",
            "memory_bytes": definition.memory_bytes,
            "cpu_millis": definition.cpu_millis,
            "pids_limit": definition.pids_limit,
            "max_stdout_bytes": definition.max_stdout_bytes,
            "max_stderr_bytes": definition.max_stderr_bytes,
        }));
        Ok(Self {
            engine,
            definition,
            implementation_digest,
        })
    }

    pub fn prepare(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
    ) -> Result<PreparedContainerLaunch, ToolExecutionError> {
        if request.sandbox != SandboxClass::RestrictedContainer {
            return Err(ToolExecutionError::WrongSandbox);
        }
        if context.timeout.is_zero() || context.timeout > Duration::from_secs(3600) {
            return Err(ToolExecutionError::InvalidContext(
                "timeout must be between 1ms and 3600 seconds".into(),
            ));
        }
        let workspace = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ToolExecutionError::InvalidContext(error.to_string()))?;
        if !workspace.is_dir() {
            return Err(ToolExecutionError::InvalidContext(
                "workspace root must be a directory".into(),
            ));
        }
        let mount = match self.definition.workspace_access {
            WorkspaceAccess::ReadOnly => {
                format!(
                    "type=bind,src={},dst=/workspace,readonly",
                    workspace.display()
                )
            }
            WorkspaceAccess::ReadWrite => {
                format!("type=bind,src={},dst=/workspace", workspace.display())
            }
        };
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--pull".into(),
            "never".into(),
            "--network".into(),
            "none".into(),
            "--read-only".into(),
            "--user".into(),
            "65532:65532".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            self.definition.pids_limit.to_string(),
            "--memory".into(),
            self.definition.memory_bytes.to_string(),
            "--cpus".into(),
            format!("{:.3}", f64::from(self.definition.cpu_millis) / 1000.0),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,size=16777216".into(),
            "--mount".into(),
            mount,
            self.definition.image.clone(),
        ];
        args.extend(self.definition.entrypoint.clone());
        Ok(PreparedContainerLaunch {
            program: self.engine.clone(),
            args,
            env: Vec::new(),
            stdin_json: json!({
                "schema_version": 1,
                "tenant_id": context.tenant_id,
                "run_id": context.run_id,
                "attempt_id": context.attempt_id,
                "requested_at": context.requested_at,
                "timeout_ms": context.timeout.as_millis(),
                "tool_call": request.call,
                "binding_digest": request.binding_digest,
            }),
        })
    }

    pub async fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        let launch = self.prepare(&request, &context)?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .env_clear()
            .envs(launch.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Same reason as the native executor: a container engine invocation is
        // a process tree, so a timeout has to reach the whole group.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolExecutionError::Engine("container stdin is unavailable".into()))?;
        let input = serde_json::to_vec(&launch.stdin_json)
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        stdin
            .write_all(&input)
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        drop(stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolExecutionError::Engine("container stdout is unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolExecutionError::Engine("container stderr is unavailable".into()))?;
        let stdout_task = tokio::spawn(capture(stdout, self.definition.max_stdout_bytes));
        let stderr_task = tokio::spawn(capture(stderr, self.definition.max_stderr_bytes));

        enum Completion {
            Exited(Result<std::process::ExitStatus, std::io::Error>),
            TimedOut,
            Cancelled,
        }
        let completion = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                status = &mut wait => Completion::Exited(status),
                () = context.cancellation.cancelled() => Completion::Cancelled,
                () = tokio::time::sleep(context.timeout) => Completion::TimedOut,
            }
        };
        let status = match completion {
            Completion::Exited(status) => {
                status.map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            }
            Completion::TimedOut => {
                reap_process_tree(&mut child).await;
                return Err(ToolExecutionError::TimedOut);
            }
            Completion::Cancelled => {
                reap_process_tree(&mut child).await;
                return Err(ToolExecutionError::Cancelled);
            }
        };
        let stdout = stdout_task
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        let stderr = stderr_task
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        if stdout.truncated || stderr.truncated {
            return Err(ToolExecutionError::OutputLimitExceeded);
        }
        if !status.success() {
            return Err(ToolExecutionError::ProcessFailed {
                exit_code: status.code().unwrap_or(128),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            });
        }
        let response: WireToolResult = serde_json::from_slice(&stdout.bytes)
            .map_err(|error| ToolExecutionError::InvalidOutput(error.to_string()))?;
        if response.tool_call_id != request.call.id
            || response.binding_digest != request.binding_digest
        {
            return Err(ToolExecutionError::OutputBindingMismatch);
        }
        Ok(ToolExecutionResult {
            content: response.content,
            is_error: response.is_error,
            exit_code: 0,
        })
    }
}

impl ToolExecutor for RestrictedContainerExecutor {
    fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        Box::pin(RestrictedContainerExecutor::execute(self, request, context))
    }
}

#[derive(Clone, Debug)]
pub struct TrustedNativeExecutor {
    definition: TrustedNativeToolDefinition,
    executable: PathBuf,
    executable_digest: String,
    implementation_digest: String,
}

impl TrustedNativeExecutor {
    pub fn new(definition: TrustedNativeToolDefinition) -> Result<Self, ToolExecutionError> {
        if definition.max_stdout_bytes == 0 || definition.max_stderr_bytes == 0 {
            return Err(ToolExecutionError::InvalidDefinition(
                "native tool output limits must be positive".into(),
            ));
        }
        let trusted_root = std::fs::canonicalize(&definition.trusted_root)
            .map_err(|error| ToolExecutionError::InvalidDefinition(error.to_string()))?;
        if !trusted_root.is_dir() {
            return Err(ToolExecutionError::InvalidDefinition(
                "native tool trusted root must be a directory".into(),
            ));
        }
        let metadata = std::fs::symlink_metadata(&definition.executable)
            .map_err(|error| ToolExecutionError::InvalidDefinition(error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ToolExecutionError::InvalidDefinition(
                "native tool executable must be a regular file, not a symlink".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ToolExecutionError::InvalidDefinition(
                    "native tool executable must have an execute bit".into(),
                ));
            }
        }
        let executable = std::fs::canonicalize(&definition.executable)
            .map_err(|error| ToolExecutionError::InvalidDefinition(error.to_string()))?;
        if !executable.starts_with(&trusted_root) {
            return Err(ToolExecutionError::InvalidDefinition(
                "native tool executable must be contained by its trusted root".into(),
            ));
        }
        let executable_digest = digest_file(&executable)
            .map_err(|error| ToolExecutionError::InvalidDefinition(error.to_string()))?;
        let implementation_digest = digest_serializable(&json!({
            "executable_digest": executable_digest,
            "fixed_args": definition.fixed_args,
            "workspace_access": "read_only",
            "max_stdout_bytes": definition.max_stdout_bytes,
            "max_stderr_bytes": definition.max_stderr_bytes,
        }));
        Ok(Self {
            definition,
            executable,
            executable_digest,
            implementation_digest,
        })
    }

    #[must_use]
    pub fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }

    pub fn prepare(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
    ) -> Result<PreparedNativeLaunch, ToolExecutionError> {
        if request.sandbox != SandboxClass::TrustedNative {
            return Err(ToolExecutionError::WrongSandbox);
        }
        if context.timeout.is_zero() || context.timeout > Duration::from_secs(3600) {
            return Err(ToolExecutionError::InvalidContext(
                "timeout must be between 1ms and 3600 seconds".into(),
            ));
        }
        self.revalidate_executable()?;
        let workspace = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ToolExecutionError::InvalidContext(error.to_string()))?;
        if !workspace.is_dir() {
            return Err(ToolExecutionError::InvalidContext(
                "workspace root must be a directory".into(),
            ));
        }
        // A trusted Tool is trusted to be the binary we registered, not trusted
        // to be free of defects. Containment is what keeps a bug or a crafted
        // argument inside the Workspace.
        #[cfg(target_os = "macos")]
        let (program, args) = {
            // Fail closed: a launch with no credential denials would look
            // contained and would not be (ADR-0037).
            let home = seatbelt::containment_home();
            let denied_reads = seatbelt::required_read_denials(home.as_deref()).map_err(|_| {
                ToolExecutionError::ContainmentUnavailable(
                    "home directory could not be resolved, so credential read containment \
                     cannot be established"
                        .into(),
                )
            })?;
            let (program, args) = seatbelt::wrap_launch(
                &self.executable,
                &self.definition.fixed_args,
                &workspace,
                self.definition.workspace_access,
                &denied_reads,
            );
            (PathBuf::from(program), args)
        };
        #[cfg(not(target_os = "macos"))]
        let (program, args) = (self.executable.clone(), self.definition.fixed_args.clone());
        Ok(PreparedNativeLaunch {
            program,
            args,
            env: Vec::new(),
            current_dir: workspace,
            stdin_json: json!({
                "schema_version": 1,
                "tenant_id": context.tenant_id,
                "run_id": context.run_id,
                "attempt_id": context.attempt_id,
                "requested_at": context.requested_at,
                "timeout_ms": context.timeout.as_millis(),
                "tool_call": request.call,
                "binding_digest": request.binding_digest,
            }),
        })
    }

    pub async fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        let launch = self.prepare(&request, &context)?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .env_clear()
            .envs(launch.env.iter().cloned())
            .current_dir(&launch.current_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout can end everything the Tool
        // started rather than only the process we hold a handle to. Before
        // `shell.exec` the two were the same thing; a shell command is
        // `sandbox-exec -> tool -> sh -c -> whatever the model wrote`, and
        // anything backgrounded behind that survived with no owner.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolExecutionError::Engine("native tool stdin is unavailable".into()))?;
        let input = serde_json::to_vec(&launch.stdin_json)
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        stdin
            .write_all(&input)
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        drop(stdin);

        let stdout = child.stdout.take().ok_or_else(|| {
            ToolExecutionError::Engine("native tool stdout is unavailable".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ToolExecutionError::Engine("native tool stderr is unavailable".into())
        })?;
        let stdout_task = tokio::spawn(capture(stdout, self.definition.max_stdout_bytes));
        let stderr_task = tokio::spawn(capture(stderr, self.definition.max_stderr_bytes));

        enum Completion {
            Exited(Result<std::process::ExitStatus, std::io::Error>),
            TimedOut,
            Cancelled,
        }
        let completion = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                status = &mut wait => Completion::Exited(status),
                () = context.cancellation.cancelled() => Completion::Cancelled,
                () = tokio::time::sleep(context.timeout) => Completion::TimedOut,
            }
        };
        let status = match completion {
            Completion::Exited(status) => {
                status.map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            }
            Completion::TimedOut => {
                reap_process_tree(&mut child).await;
                return Err(ToolExecutionError::TimedOut);
            }
            Completion::Cancelled => {
                reap_process_tree(&mut child).await;
                return Err(ToolExecutionError::Cancelled);
            }
        };
        let stdout = stdout_task
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        let stderr = stderr_task
            .await
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?
            .map_err(|error| ToolExecutionError::Engine(error.to_string()))?;
        if stdout.truncated || stderr.truncated {
            return Err(ToolExecutionError::OutputLimitExceeded);
        }
        if !status.success() {
            return Err(ToolExecutionError::ProcessFailed {
                exit_code: status.code().unwrap_or(128),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            });
        }
        let response: WireToolResult = serde_json::from_slice(&stdout.bytes)
            .map_err(|error| ToolExecutionError::InvalidOutput(error.to_string()))?;
        if response.tool_call_id != request.call.id
            || response.binding_digest != request.binding_digest
        {
            return Err(ToolExecutionError::OutputBindingMismatch);
        }
        Ok(ToolExecutionResult {
            content: response.content,
            is_error: response.is_error,
            exit_code: 0,
        })
    }

    fn revalidate_executable(&self) -> Result<(), ToolExecutionError> {
        let metadata = std::fs::symlink_metadata(&self.definition.executable)
            .map_err(|_| ToolExecutionError::ExecutableChanged)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ToolExecutionError::ExecutableChanged);
        }
        let canonical = std::fs::canonicalize(&self.definition.executable)
            .map_err(|_| ToolExecutionError::ExecutableChanged)?;
        if canonical != self.executable
            || digest_file(&canonical).map_err(|_| ToolExecutionError::ExecutableChanged)?
                != self.executable_digest
        {
            return Err(ToolExecutionError::ExecutableChanged);
        }
        Ok(())
    }
}

impl ToolExecutor for TrustedNativeExecutor {
    fn implementation_digest(&self) -> &str {
        self.implementation_digest()
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        Box::pin(TrustedNativeExecutor::execute(self, request, context))
    }
}

/// Ends the child and everything it started.
///
/// `Child::kill` signals one pid. A Tool that spawned anything -- the normal
/// case for `shell.exec` -- leaves those processes running with no owner and
/// nothing watching them. Signalling the group reaches the whole tree.
///
/// Approach adapted from OpenAI Codex (Apache-2.0),
/// `codex-rs/utils/pty/src/process_group.rs`: the child leads its own group via
/// `process_group(0)`, and the group is signalled by its id. Codex escalates
/// SIGTERM to SIGKILL; this sends SIGKILL directly, because a Tool that has hit
/// its timeout or been cancelled has already lost its chance to exit tidily,
/// and the platform's ambiguous-failure rule governs what it left behind.
async fn reap_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The group is signalled *before* waiting: killing only the direct
        // child would leave grandchildren holding the stdout pipe, and the wait
        // below would then block on a tree that is still alive.
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        // Never signal our own group -- that would take the Worker with it.
        if pgid > 0 && pgid != unsafe { libc::getpgrp() } {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn digest_file(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn digest_serializable(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value).expect("tool implementation identity is serializable");
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Captured, std::io::Error> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok(Captured { bytes, truncated })
}

#[derive(Debug, Deserialize, Serialize)]
struct WireToolResult {
    tool_call_id: String,
    binding_digest: String,
    content: Value,
    is_error: bool,
}

fn validate_definition(
    engine: &str,
    definition: &ContainerToolDefinition,
) -> Result<(), ToolExecutionError> {
    if engine.trim().is_empty() || !std::path::Path::new(engine).is_absolute() {
        return Err(ToolExecutionError::InvalidDefinition(
            "container engine must use an absolute path".into(),
        ));
    }
    let Some((repository, digest)) = definition.image.rsplit_once("@sha256:") else {
        return Err(ToolExecutionError::InvalidDefinition(
            "image must be pinned by sha256 digest".into(),
        ));
    };
    if repository.is_empty()
        || repository.chars().any(char::is_whitespace)
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ToolExecutionError::InvalidDefinition(
            "image must be pinned by lowercase sha256 digest".into(),
        ));
    }
    if definition.entrypoint.is_empty()
        || !definition.entrypoint[0].starts_with('/')
        || definition.entrypoint.iter().any(|part| part.is_empty())
    {
        return Err(ToolExecutionError::InvalidDefinition(
            "entrypoint must use an absolute executable without empty arguments".into(),
        ));
    }
    if definition.memory_bytes == 0
        || definition.cpu_millis == 0
        || definition.pids_limit == 0
        || definition.max_stdout_bytes == 0
        || definition.max_stderr_bytes == 0
    {
        return Err(ToolExecutionError::InvalidDefinition(
            "resource and output limits must be positive".into(),
        ));
    }
    Ok(())
}
