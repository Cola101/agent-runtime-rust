#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "Linux cgroup protocol stays unreachable until persisted recovery wiring is complete"
    )
)]
mod process_resources;
mod process_session;
#[cfg(target_os = "macos")]
mod seatbelt;

pub use process_session::{
    PROCESS_ATTACH_TOOL, PROCESS_CLOSE_TOOL, PROCESS_INTERRUPT_TOOL, PROCESS_POLL_TOOL,
    PROCESS_RESIZE_TOOL, PROCESS_START_TOOL, PROCESS_WAIT_TOOL, PROCESS_WRITE_TOOL,
    PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionError, ProcessSessionGovernance, ProcessSessionInteraction, ProcessSessionOutput,
    ProcessSessionPtySupervisorConfig, ProcessSessionQuotaScope, ProcessSessionRecovery,
    ProcessSessionResourceBackendConfig, ProcessSessionResourceBackendKind,
    ProcessSessionResourceCapabilities, ProcessSessionStartRequest, ProcessSessionState,
    ProcessSessionSweepReport, ProcessSessionTerminationReason, ProcessSessionToolExecutor,
    ProcessSessionToolOperation, ProcessWaitObservationSnapshot,
};

#[cfg(unix)]
pub use process_session::run_process_session_pty_supervisor;

use agent_protocol::{
    McpElicitationRequest, McpInputContinuation, SandboxClass, ToolCall, ToolExecutionRequest,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceAccess {
    ReadOnly,
    /// Writes are permitted, but only beneath the canonical Workspace root and
    /// only because containment enforces that boundary.
    ReadWrite,
}

/// Which containment mechanism, if any, this build can apply to a trusted
/// native Tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContainmentBackendKind {
    /// macOS Seatbelt, applied per launch through `sandbox-exec` (ADR-0037).
    MacosSeatbelt,
    /// No containment backend exists here. `TrustedNative` cannot be honoured.
    Unsupported,
}

/// What the containment backend actually guarantees, stated per guarantee
/// rather than per platform.
///
/// This exists for the same reason [`ProcessSessionResourceCapabilities`] does
/// (ADR-0072): a boundary that is absent must be *typed and refused*, never
/// silently skipped. Before this type existed, a non-macOS build ran a
/// `TrustedNative` Tool with no containment at all while the descriptor, the
/// implementation digest and the approval record all still said
/// `TrustedNative`. Nothing reported the difference, so the only thing
/// standing between that and an escape was a sentence in a document.
///
/// Every field is a guarantee the *operating system* enforces. A Tool
/// promising not to do something is not a capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ToolContainmentCapabilities {
    pub backend: ToolContainmentBackendKind,
    /// Writes outside the canonical Workspace root are refused by the kernel.
    pub workspace_write_confinement: bool,
    /// Reads of the credential directories are refused by the kernel.
    pub credential_read_denial: bool,
    /// Outbound network access is refused by the kernel.
    pub network_egress_denial: bool,
}

impl ToolContainmentCapabilities {
    /// What this build can enforce.
    ///
    /// `const` and derived from `cfg!` so it cannot drift from what the
    /// launch path is actually compiled to do.
    #[must_use]
    pub const fn current() -> Self {
        let seatbelt = cfg!(target_os = "macos");
        Self {
            backend: if seatbelt {
                ToolContainmentBackendKind::MacosSeatbelt
            } else {
                ToolContainmentBackendKind::Unsupported
            },
            workspace_write_confinement: seatbelt,
            credential_read_denial: seatbelt,
            network_egress_denial: seatbelt,
        }
    }
}

/// Refuses a launch whose containment guarantees are not all present.
///
/// Called before the Workspace is canonicalised and before any process is
/// created, so an uncontained host fails at the contract rather than part-way
/// into a launch. The error names the first missing guarantee; it does not
/// name the platform, because what matters to the caller is which boundary
/// they were promised and did not get.
pub fn validate_containment(
    capabilities: ToolContainmentCapabilities,
) -> Result<(), ToolExecutionError> {
    if capabilities.backend == ToolContainmentBackendKind::Unsupported {
        return Err(ToolExecutionError::UnsupportedContainment(
            "containment_backend",
        ));
    }
    if !capabilities.workspace_write_confinement {
        return Err(ToolExecutionError::UnsupportedContainment(
            "workspace_write_confinement",
        ));
    }
    if !capabilities.credential_read_denial {
        return Err(ToolExecutionError::UnsupportedContainment(
            "credential_read_denial",
        ));
    }
    if !capabilities.network_egress_denial {
        return Err(ToolExecutionError::UnsupportedContainment(
            "network_egress_denial",
        ));
    }
    Ok(())
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
    pub application_id: Uuid,
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
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

/// One bounded, advisory update emitted while a Tool request is still active.
/// Progress is never authority for replay or completion; the durable Tool
/// result and Run terminal event remain the only completion boundaries.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolExecutionProgress {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Non-blocking progress sink. A noisy Tool cannot stall execution or grow an
/// unbounded queue: once the receiver falls behind, intermediate updates are
/// deliberately dropped while the final result continues normally.
#[derive(Clone, Debug)]
pub struct ToolProgressReporter {
    sender: Option<mpsc::Sender<ToolExecutionProgress>>,
}

impl ToolProgressReporter {
    #[must_use]
    pub fn disabled() -> Self {
        Self { sender: None }
    }

    #[must_use]
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<ToolExecutionProgress>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (
            Self {
                sender: Some(sender),
            },
            receiver,
        )
    }

    pub fn try_report(&self, progress: ToolExecutionProgress) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(progress);
        }
    }
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
    #[error("this host cannot enforce required tool containment: {0}")]
    UnsupportedContainment(&'static str),
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
    #[error("persistent process session {session_id} start failed: {reason}")]
    ProcessSessionStartFailed { session_id: Uuid, reason: String },
    #[error("persistent process session failed: {0}")]
    PersistentProcessSession(String),
    #[error("MCP Tool requires recoverable user input at round {round}")]
    McpInputRequired {
        round: u8,
        request_state: String,
        requests: std::collections::BTreeMap<String, McpElicitationRequest>,
    },
}

impl ToolExecutionError {
    /// Converts only failures that prove execution never crossed the external
    /// side-effect boundary into a model-visible Tool result. Callers must not
    /// use this as a generic error adapter: an unclassified failure of a
    /// non-idempotent Tool remains ambiguous and needs recovery/reconciliation.
    #[must_use]
    pub fn deterministic_failure_result(&self) -> Option<ToolExecutionResult> {
        match self {
            Self::ProcessSessionStartFailed { session_id, .. } => Some(ToolExecutionResult {
                content: json!({
                    "error": {
                        "code": "process_session_start_failed",
                        "message": "persistent process session could not be started",
                        "session_id": session_id,
                    }
                }),
                is_error: true,
                exit_code: 1,
            }),
            // The refusal happens before the Workspace is resolved and before
            // any process is created (ADR-0122), so this provably never
            // crossed the side-effect boundary. Without this arm a
            // NonIdempotent Tool on an uncontained host would converge to
            // `indeterminate` -- claiming "it might have run" about something
            // that demonstrably did not, and sending an operator to reconcile
            // an effect that never existed.
            //
            // The missing guarantee is deliberately NOT named here. It reaches
            // the operator through the error's Display and the event code; the
            // model only needs to know this Tool cannot run on this host.
            Self::UnsupportedContainment(_) => Some(ToolExecutionResult {
                content: json!({
                    "error": {
                        "code": "tool_containment_unsupported",
                        "message": "this host cannot enforce the containment this Tool requires",
                    }
                }),
                is_error: true,
                exit_code: 1,
            }),
            _ => None,
        }
    }
}

pub trait ToolExecutor: Send + Sync {
    fn implementation_digest(&self) -> &str;

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>;

    fn execute_with_progress(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
        _progress: ToolProgressReporter,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        self.execute(request, context)
    }

    /// Observes a previously accepted non-idempotent execution without
    /// repeating its side effect. Implementations may return a bound result
    /// only when durable executor-owned state proves the original operation;
    /// the default keeps the Run indeterminate.
    fn recover_started_result(
        &self,
        _request: ToolExecutionRequest,
        _context: ToolExecutionContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<ToolExecutionResult>, ToolExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Ok(None) })
    }

    fn resume_with_mcp_input(
        &self,
        _request: ToolExecutionRequest,
        _context: ToolExecutionContext,
        _continuation: McpInputContinuation,
        _progress: ToolProgressReporter,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>>
    {
        Box::pin(async {
            Err(ToolExecutionError::InvalidContext(
                "tool executor does not support MCP continuation".into(),
            ))
        })
    }
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
        // Same defect as the trusted native executor had: this said
        // "read_only" unconditionally while `prepare` builds a writable bind
        // mount from the real value, so read-only and read-write container
        // Tools were indistinguishable by digest.
        let implementation_digest = digest_serializable(&json!({
            "image": definition.image,
            "entrypoint": definition.entrypoint,
            "workspace_access": match definition.workspace_access {
                WorkspaceAccess::ReadOnly => "read_only",
                WorkspaceAccess::ReadWrite => "read_write",
            },
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
                "application_id": context.application_id,
                "workload_identity_id": context.workload_identity_id,
                "run_id": context.run_id,
                "session_id": context.session_id,
                "workspace_id": context.workspace_id,
                "agent_version_id": context.agent_version_id,
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
    /// What each Run has seen in the workspace, so a later write by the same
    /// Run can say what it expected to find.
    ///
    /// The tool refuses a write whose `expected_sha256` no longer matches, and
    /// until this existed nothing ever sent that field -- the check was there
    /// and could not fire. What it protects against is ordinary rather than
    /// adversarial: a person edits a file while an approval for a write to it
    /// is on screen, and the write lands on top with nothing recording that
    /// anything was lost.
    ///
    /// Keyed by Run because two Runs are two accounts. What one Run read says
    /// nothing about what another may overwrite, and sharing the entry would
    /// refuse a write for a reason that has nothing to do with the Run being
    /// refused.
    seen: Arc<Mutex<SeenFiles>>,
}

/// How many (Run, path) pairs one executor remembers.
///
/// A bound rather than a policy: this map lives for the life of the process and
/// a long-running host would otherwise hold one entry per file per Run for
/// ever. Reaching it means the oldest entry is dropped, and dropping one is
/// safe in the direction that matters -- a write with no expectation behaves
/// exactly as it did before any of this, which is to say it writes.
const MAX_SEEN_FILES: usize = 4_096;

/// The longest path this executor will remember.
///
/// The key is a string taken from the Tool's own stdout, so without a cap here
/// the bound above is a count of entries and not a bound on memory: one entry
/// could be as long as `max_stdout_bytes`. `PATH_MAX` on the platforms this
/// runs on is 1024, so a longer one is not a path this Tool could have opened.
const MAX_SEEN_PATH_BYTES: usize = 1024;

/// What each Run has read, oldest first.
///
/// The order is kept explicitly rather than read off the map: `HashMap` has no
/// order, so evicting `keys().next()` drops an arbitrary entry -- which at
/// capacity can be the read whose write is the next thing to happen. That was
/// what the first version of this did while its comment said "oldest".
#[derive(Debug, Default)]
struct SeenFiles {
    digests: HashMap<(Uuid, String), String>,
    order: VecDeque<(Uuid, String)>,
}

impl SeenFiles {
    fn get(&self, key: &(Uuid, String)) -> Option<&String> {
        self.digests.get(key)
    }

    fn insert(&mut self, key: (Uuid, String), digest: String) {
        if self.digests.insert(key.clone(), digest).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > MAX_SEEN_FILES {
            if let Some(oldest) = self.order.pop_front() {
                self.digests.remove(&oldest);
            }
        }
    }
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
        // `workspace_access` was a hardcoded "read_only" here while the launch
        // path honoured the real value, so a Tool that could write the
        // Workspace and one that could not produced the *same* digest. The
        // containment capabilities join it for the same reason: the digest is
        // what proves two implementations are the same thing, and a boundary
        // the host cannot enforce makes them different things.
        let implementation_digest = digest_serializable(&json!({
            "executable_digest": executable_digest,
            "fixed_args": definition.fixed_args,
            "workspace_access": match definition.workspace_access {
                WorkspaceAccess::ReadOnly => "read_only",
                WorkspaceAccess::ReadWrite => "read_write",
            },
            "max_stdout_bytes": definition.max_stdout_bytes,
            "max_stderr_bytes": definition.max_stderr_bytes,
            "containment": Self::containment_capabilities(),
        }));
        Ok(Self {
            definition,
            executable,
            executable_digest,
            implementation_digest,
            seen: Arc::new(Mutex::new(SeenFiles::default())),
        })
    }

    /// What this Run last saw at this path, if it has seen it.
    fn expectation(&self, run: Uuid, path: &str) -> Option<String> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(run, path.to_owned()))
            .cloned()
    }

    /// Records what this Run now knows a path to hold.
    ///
    /// Called after a read *and* after a write: a Run that writes twice must
    /// carry what its own first write left, or the second would be refused for
    /// a change the Run made itself.
    fn remember(&self, run: Uuid, path: &str, text: &str) {
        // A path longer than any this Tool could have opened is not remembered
        // at all, rather than remembered and counted against the bound.
        if path.len() > MAX_SEEN_PATH_BYTES {
            return;
        }
        // Bounded by dropping rather than by refusing. An entry lost means a
        // later write carries no expectation, which is exactly how every write
        // behaved before this existed.
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((run, path.to_owned()), digest_bytes(text.as_bytes()));
    }

    #[must_use]
    pub fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }

    /// What this build can enforce when it launches a trusted native Tool.
    #[must_use]
    pub const fn containment_capabilities() -> ToolContainmentCapabilities {
        ToolContainmentCapabilities::current()
    }

    /// Applies the platform containment backend to a launch.
    ///
    /// The unsupported arm returns an error rather than the bare executable.
    /// That is the whole point: the previous shape returned
    /// `(executable, fixed_args)` unchanged on any non-macOS host, so the Tool
    /// ran with no containment while every record still said `TrustedNative`.
    #[cfg(target_os = "macos")]
    fn wrap_with_containment(
        &self,
        workspace: &Path,
    ) -> Result<(PathBuf, Vec<String>), ToolExecutionError> {
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
            workspace,
            self.definition.workspace_access,
            &denied_reads,
        );
        Ok((PathBuf::from(program), args))
    }

    #[cfg(not(target_os = "macos"))]
    fn wrap_with_containment(
        &self,
        _workspace: &Path,
    ) -> Result<(PathBuf, Vec<String>), ToolExecutionError> {
        Err(ToolExecutionError::UnsupportedContainment(
            "containment_backend",
        ))
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
        // Refuse before the Workspace is resolved and before anything is
        // spawned. A host that cannot contain this Tool must not get as far as
        // looking like it is about to run it.
        validate_containment(Self::containment_capabilities())?;
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
        let (program, args) = self.wrap_with_containment(&workspace)?;
        Ok(PreparedNativeLaunch {
            program,
            args,
            env: Vec::new(),
            current_dir: workspace,
            stdin_json: json!({
                "schema_version": 1,
                "tenant_id": context.tenant_id,
                "application_id": context.application_id,
                "workload_identity_id": context.workload_identity_id,
                "run_id": context.run_id,
                "session_id": context.session_id,
                "workspace_id": context.workspace_id,
                "agent_version_id": context.agent_version_id,
                "attempt_id": context.attempt_id,
                "requested_at": context.requested_at,
                "timeout_ms": context.timeout.as_millis(),
                "tool_call": self.with_expectation(&request.call, context),
                "binding_digest": request.binding_digest,
            }),
        })
    }

    /// The call as the tool will receive it, with what this Run expects to find
    /// at the path when it is writing to one it has read.
    ///
    /// Added here rather than by the caller because the caller does not know
    /// what this Run has read -- and must not, since the point of keeping it
    /// here is that the model cannot decline to send it.
    ///
    /// The field is this executor's alone in both directions: a value the model
    /// put in the arguments is dropped whether or not one is added back. A
    /// model that could set it could not widen anything -- the check only ever
    /// refuses -- but it could make its own writes fail for a reason nobody
    /// could see, and "the tool refused" is not a sentence worth sharing
    /// between a guarantee and a model's mistake.
    ///
    /// One consequence to state plainly: the arguments the tool executes are no
    /// longer byte-identical to the arguments the binding digest was computed
    /// over, because that digest is fixed when the approval is issued and this
    /// is added when the call is launched. Nothing recomputes the digest from
    /// what the tool received, so nothing breaks today -- but the difference is
    /// real, and it is only acceptable in this direction: the added field can
    /// refuse the approved write and can never turn it into a different one.
    fn with_expectation(&self, call: &ToolCall, context: &ToolExecutionContext) -> Value {
        let mut wire = serde_json::to_value(call).expect("a tool call is serializable");
        let expected = if call.name == "workspace.write_text" {
            call.arguments
                .get("path")
                .and_then(Value::as_str)
                // No expectation means this Run never read the path: it is
                // creating a file, or deliberately replacing one nobody looked
                // at. An expectation invented here would refuse an act the
                // person is entitled to.
                .and_then(|path| self.expectation(context.run_id, path))
        } else {
            None
        };
        if let Some(arguments) = wire.get_mut("arguments").and_then(Value::as_object_mut) {
            arguments.remove("expected_sha256");
            if let Some(expected) = expected {
                arguments.insert("expected_sha256".into(), Value::String(expected));
            }
        }
        wire
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
        // What this Run now knows the file to hold. Recorded from what the tool
        // actually returned rather than from what was asked for, so a write the
        // tool altered or refused does not leave a claim behind.
        if !response.is_error
            && matches!(
                request.call.name.as_str(),
                "workspace.read_text" | "workspace.write_text"
            )
            && let Some(path) = response.content.get("path").and_then(Value::as_str)
            && let Some(text) = response.content.get("text").and_then(Value::as_str)
        {
            self.remember(context.run_id, path, text);
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
        // Spawn used `process_group(0)`, so the child PID is the group ID by
        // construction. Re-resolving it with `getpgid(pid)` creates a race with
        // leader exit and PID reuse that can skip the still-live descendants.
        let pgid = pid as libc::pid_t;
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

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
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

#[cfg(test)]
mod seen_files_tests {
    use super::{MAX_SEEN_FILES, SeenFiles};
    use uuid::Uuid;

    /// The bound must drop the oldest entry, not an arbitrary one.
    ///
    /// The first version read `keys().next()` off a `HashMap` and called it the
    /// oldest. At capacity that can discard the read whose write is the next
    /// thing to happen -- so the guard would go quiet on exactly the file being
    /// worked on, and nothing would say it had.
    #[test]
    fn the_bound_drops_the_oldest_and_keeps_the_newest() {
        let run = Uuid::now_v7();
        let mut seen = SeenFiles::default();
        for index in 0..MAX_SEEN_FILES + 8 {
            seen.insert((run, format!("file-{index}")), format!("digest-{index}"));
        }
        assert_eq!(seen.digests.len(), MAX_SEEN_FILES);
        for index in 0..8 {
            assert!(
                seen.get(&(run, format!("file-{index}"))).is_none(),
                "file-{index} was inserted first and must be the first to go",
            );
        }
        for index in 8..MAX_SEEN_FILES + 8 {
            assert_eq!(
                seen.get(&(run, format!("file-{index}"))),
                Some(&format!("digest-{index}")),
                "file-{index} is newer than what was dropped",
            );
        }
    }

    /// Writing the same path twice must not queue it twice, or the order would
    /// hold stale keys and the map would be evicted below its own bound.
    #[test]
    fn rewriting_one_path_does_not_take_a_second_place_in_the_queue() {
        let run = Uuid::now_v7();
        let mut seen = SeenFiles::default();
        seen.insert((run, "notes.txt".into()), "one".into());
        seen.insert((run, "notes.txt".into()), "two".into());
        assert_eq!(seen.order.len(), 1);
        assert_eq!(seen.get(&(run, "notes.txt".into())), Some(&"two".to_string()));
    }
}
