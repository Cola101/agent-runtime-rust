use crate::{
    ToolExecutionContext, ToolExecutionError, ToolExecutionResult, ToolExecutor,
    TrustedNativeExecutor,
    process_resources::{
        LinuxCgroupV2Group, LinuxCgroupV2Root, ProcessResourceError,
        install_linux_cgroup_membership_group, kill_linux_cgroup_v2_group,
        prepare_linux_cgroup_v2_root, read_linux_cgroup_cpu_usage_micros_group,
        read_linux_cgroup_populated_group, remove_linux_cgroup_v2_group_root,
    },
};
use agent_protocol::ToolExecutionRequest;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

#[cfg(unix)]
mod pty_supervisor;
#[cfg(unix)]
pub use pty_supervisor::run_process_session_pty_supervisor;

const PROCESS_SESSION_SCHEMA_VERSION: u32 = 7;
const PROCESS_SESSION_TOOL_IMPLEMENTATION_VERSION: u32 = 10;
const MAX_PROCESS_SESSIONS: usize = 64;
const MAX_STDIN_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_CHUNK_BYTES: usize = 1024 * 1024;
const CLOSE_GRACE: Duration = Duration::from_millis(500);
const MAX_GOVERNANCE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MIN_GOVERNANCE_CHECK_INTERVAL: Duration = Duration::from_millis(10);
const RESOURCE_IDENTITY_TRANSITION_GRACE: Duration = Duration::from_secs(1);
const PROCESS_WAIT_OBSERVATION_INTERVAL: Duration = Duration::from_millis(50);
static PROCESS_SESSION_SPAWN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub const PROCESS_START_TOOL: &str = "process.start";
pub const PROCESS_WRITE_TOOL: &str = "process.write";
pub const PROCESS_RESIZE_TOOL: &str = "process.resize";
pub const PROCESS_POLL_TOOL: &str = "process.poll";
pub const PROCESS_ATTACH_TOOL: &str = "process.attach";
pub const PROCESS_WAIT_TOOL: &str = "process.wait";
pub const PROCESS_INTERRUPT_TOOL: &str = "process.interrupt";
pub const PROCESS_CLOSE_TOOL: &str = "process.close";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionAccess {
    pub tenant_id: Uuid,
    pub workspace_root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProcessSessionStartRequest {
    pub session_id: Uuid,
    pub request: ToolExecutionRequest,
    pub context: ToolExecutionContext,
    pub initial_stdin: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionPtySupervisorConfig {
    pub executable: PathBuf,
    pub fixed_args: Vec<String>,
    pub startup_timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessSessionAction {
    Poll,
    Attach { max_bytes: usize },
    Write { bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Interrupt,
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSessionInteraction {
    pub session_id: Uuid,
    pub stdout_cursor: u64,
    pub stderr_cursor: u64,
    pub action: ProcessSessionAction,
}

struct ProcessYieldRequest {
    initial_output: Option<ProcessSessionOutput>,
    stdout_cursor: u64,
    stderr_cursor: u64,
    requested_wait: Duration,
    execution_started_at: tokio::time::Instant,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSessionState {
    Starting,
    Running,
    Terminating,
    Exited,
    Terminated,
    Indeterminate,
}

impl ProcessSessionState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Terminated | Self::Indeterminate)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionRecovery {
    Reattached,
    Terminated,
    Indeterminate,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessSessionSweepReport {
    pub examined: usize,
    pub active: usize,
    pub terminated: usize,
    pub indeterminate: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessSessionOutput {
    pub session_id: Uuid,
    pub state: ProcessSessionState,
    pub pid: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_start_cursor: u64,
    pub stderr_start_cursor: u64,
    pub stdout_cursor: u64,
    pub stderr_cursor: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub exit_code: Option<i32>,
    pub termination_reason: Option<ProcessSessionTerminationReason>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSessionTerminationReason {
    CpuLimit,
    ExecutionDeadline,
    IdleTimeout,
    OutputLimit,
    StartFailed,
    Closed,
    RecoveredMissing,
    LegacyTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionQuotaScope {
    Global,
    Tenant,
    Workspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionResourceBackendKind {
    UnixRlimit,
    LinuxCgroupV2,
    Unsupported,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessSessionResourceBackendConfig {
    #[default]
    UnixRlimit,
    LinuxCgroupV2 {
        delegated_root: PathBuf,
    },
}

#[derive(Clone)]
enum ProcessSessionResourceBackend {
    UnixRlimit,
    LinuxCgroupV2 { root: Arc<LinuxCgroupV2Root> },
}

impl ProcessSessionResourceBackend {
    fn open(config: &ProcessSessionResourceBackendConfig) -> Result<Self, ProcessSessionError> {
        match config {
            ProcessSessionResourceBackendConfig::UnixRlimit => Ok(Self::UnixRlimit),
            ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root } => {
                let root = LinuxCgroupV2Root::open(delegated_root).map_err(|error| {
                    ProcessSessionError::InvalidConfiguration(error.to_string())
                })?;
                Ok(Self::LinuxCgroupV2 {
                    root: Arc::new(root),
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSessionResourceCapabilities {
    pub backend: ProcessSessionResourceBackendKind,
    pub hard_output_file_limit: bool,
    pub hard_cpu_time_limit: bool,
    pub hard_memory_limit: bool,
    pub hard_process_count_limit: bool,
    pub whole_process_tree_accounting: bool,
}

impl ProcessSessionResourceCapabilities {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            backend: if cfg!(unix) {
                ProcessSessionResourceBackendKind::UnixRlimit
            } else {
                ProcessSessionResourceBackendKind::Unsupported
            },
            hard_output_file_limit: cfg!(unix),
            hard_cpu_time_limit: cfg!(unix),
            hard_memory_limit: cfg!(all(unix, not(target_os = "macos"))),
            hard_process_count_limit: false,
            whole_process_tree_accounting: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessSessionGovernance {
    pub resource_backend: ProcessSessionResourceBackendConfig,
    pub max_active_sessions: usize,
    pub max_active_sessions_per_tenant: usize,
    pub max_active_sessions_per_workspace: usize,
    pub max_runtime: Duration,
    pub idle_timeout: Duration,
    pub max_output_bytes_per_stream: u64,
    pub max_cpu_seconds: u64,
    pub max_memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    pub require_whole_process_tree_accounting: bool,
}

impl Default for ProcessSessionGovernance {
    fn default() -> Self {
        Self {
            resource_backend: ProcessSessionResourceBackendConfig::default(),
            max_active_sessions: MAX_PROCESS_SESSIONS,
            max_active_sessions_per_tenant: MAX_PROCESS_SESSIONS,
            max_active_sessions_per_workspace: MAX_PROCESS_SESSIONS,
            max_runtime: Duration::from_secs(24 * 60 * 60),
            idle_timeout: Duration::from_secs(30 * 60),
            max_output_bytes_per_stream: 64 * 1024 * 1024,
            max_cpu_seconds: 10 * 60,
            max_memory_bytes: if cfg!(target_os = "macos") {
                None
            } else {
                Some(4 * 1024 * 1024 * 1024)
            },
            max_processes: None,
            require_whole_process_tree_accounting: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProcessSessionError {
    #[error("invalid persistent process session configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid persistent process session request: {0}")]
    InvalidRequest(String),
    #[error("persistent process session does not exist")]
    NotFound,
    #[error("persistent process session belongs to another tenant or Workspace")]
    AccessDenied,
    #[error("persistent process session identity already exists")]
    Conflict,
    #[error("persistent process session output cursor is invalid")]
    InvalidCursor,
    #[error("persistent process session identity is ambiguous")]
    Indeterminate,
    #[error("persistent process session {0:?} quota is exhausted")]
    QuotaExceeded(ProcessSessionQuotaScope),
    #[error("persistent process session resource capability is unavailable: {0}")]
    UnsupportedResourceCapability(&'static str),
    #[error("persistent process session {session_id} start failed: {reason}")]
    StartFailed { session_id: Uuid, reason: String },
    #[error("persistent process session I/O failed: {0}")]
    Io(String),
    #[error("persistent process sessions are unsupported on this platform")]
    UnsupportedPlatform,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProcessSessionManifest {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    governance_digest: String,
    resource_identity: ProcessSessionResourceIdentity,
    resource_phase: ProcessSessionResourcePhase,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: DateTime<Utc>,
    execution_deadline_at: DateTime<Utc>,
    idle_timeout_millis: u64,
    last_activity_at: DateTime<Utc>,
    max_output_bytes_per_stream: u64,
    max_cpu_seconds: u64,
    max_memory_bytes: Option<u64>,
    observed_cpu_usage_micros: u64,
    observed_stdout_bytes: u64,
    observed_stderr_bytes: u64,
    termination_reason: Option<ProcessSessionTerminationReason>,
    updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProcessSessionResourceIdentity {
    UnixRlimit,
    LinuxCgroupV2 { group_name: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProcessSessionResourcePhase {
    Unprepared,
    Prepared,
    Active,
    CleanupPending,
    Cleaned,
    LegacyUnknown,
}

fn terminal_resource_phase(
    resource_identity: &ProcessSessionResourceIdentity,
) -> ProcessSessionResourcePhase {
    match resource_identity {
        ProcessSessionResourceIdentity::UnixRlimit => ProcessSessionResourcePhase::Cleaned,
        ProcessSessionResourceIdentity::LinuxCgroupV2 { .. } => {
            ProcessSessionResourcePhase::CleanupPending
        }
    }
}

impl ProcessSessionResourceIdentity {
    fn for_backend(backend: &ProcessSessionResourceBackend, session_id: Uuid) -> Self {
        match backend {
            ProcessSessionResourceBackend::UnixRlimit => Self::UnixRlimit,
            ProcessSessionResourceBackend::LinuxCgroupV2 { .. } => Self::LinuxCgroupV2 {
                group_name: format!("session-{session_id}"),
            },
        }
    }

    fn is_well_formed_for(&self, session_id: Uuid) -> bool {
        match self {
            Self::UnixRlimit => true,
            Self::LinuxCgroupV2 { group_name } => *group_name == format!("session-{session_id}"),
        }
    }
}

impl ProcessSessionManifest {
    fn is_well_formed(&self) -> bool {
        self.schema_version == PROCESS_SESSION_SCHEMA_VERSION
            && !self.session_id.is_nil()
            && !self.tenant_id.is_nil()
            && self.workspace_root.is_absolute()
            && !self.source_run_id.is_nil()
            && !self.source_attempt_id.is_nil()
            && !self.source_tool_call_id.trim().is_empty()
            && self.source_tool_call_id.len() <= 256
            && is_sha256(&self.source_binding_digest)
            && is_sha256(&self.implementation_digest)
            && is_sha256(&self.governance_digest)
            && self.resource_identity.is_well_formed_for(self.session_id)
            && matches!(
                (&self.resource_identity, self.resource_phase),
                (
                    ProcessSessionResourceIdentity::UnixRlimit,
                    ProcessSessionResourcePhase::Unprepared
                        | ProcessSessionResourcePhase::Prepared
                        | ProcessSessionResourcePhase::Active
                        | ProcessSessionResourcePhase::Cleaned
                        | ProcessSessionResourcePhase::LegacyUnknown,
                ) | (ProcessSessionResourceIdentity::LinuxCgroupV2 { .. }, _)
            )
            && self.operation_sequence > 0
            && !self.last_operation.trim().is_empty()
            && self
                .last_input_digest
                .as_ref()
                .is_none_or(|digest| is_sha256(digest))
            && self.execution_deadline_at >= self.started_at
            && self.idle_timeout_millis > 0
            && self.last_activity_at >= self.started_at
            && self.last_activity_at <= self.updated_at
            && self.max_output_bytes_per_stream > 0
            && self.max_cpu_seconds > 0
            && self.max_memory_bytes.is_none_or(|bytes| bytes > 0)
            && (matches!(
                self.resource_identity,
                ProcessSessionResourceIdentity::LinuxCgroupV2 { .. }
            ) || self.observed_cpu_usage_micros == 0)
            && self.updated_at >= self.started_at
            && match self.state {
                ProcessSessionState::Starting => {
                    self.pid.is_none()
                        && self.process_group_id.is_none()
                        && self.termination_reason.is_none()
                        && matches!(
                            self.resource_phase,
                            ProcessSessionResourcePhase::Unprepared
                                | ProcessSessionResourcePhase::Prepared
                                | ProcessSessionResourcePhase::LegacyUnknown
                        )
                }
                ProcessSessionState::Running | ProcessSessionState::Terminating => {
                    self.pid.is_some()
                        && self.process_group_id == self.pid.and_then(|pid| i32::try_from(pid).ok())
                        && self.exit_code.is_none()
                        && (self.state == ProcessSessionState::Terminating
                            || self.termination_reason.is_none())
                        && (self.state != ProcessSessionState::Terminating
                            || self.termination_reason.is_some())
                        && self.resource_phase == ProcessSessionResourcePhase::Active
                }
                ProcessSessionState::Exited => {
                    self.pid.is_none()
                        && self.process_group_id.is_none()
                        && self.termination_reason.is_none()
                        && matches!(
                            self.resource_phase,
                            ProcessSessionResourcePhase::CleanupPending
                                | ProcessSessionResourcePhase::Cleaned
                        )
                }
                ProcessSessionState::Terminated => {
                    self.pid.is_none()
                        && self.process_group_id.is_none()
                        && self.termination_reason.is_some()
                        && matches!(
                            self.resource_phase,
                            ProcessSessionResourcePhase::CleanupPending
                                | ProcessSessionResourcePhase::Cleaned
                        )
                }
                ProcessSessionState::Indeterminate => {
                    self.pid.is_none()
                        && self.process_group_id.is_none()
                        && self.termination_reason.is_none()
                        && matches!(
                            self.resource_phase,
                            ProcessSessionResourcePhase::CleanupPending
                                | ProcessSessionResourcePhase::Cleaned
                        )
                }
            }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedProcessSessionManifest {
    manifest: ProcessSessionManifest,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LegacyProcessSessionManifestV3 {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    governance_digest: String,
    resource_identity: ProcessSessionResourceIdentity,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: DateTime<Utc>,
    execution_deadline_at: DateTime<Utc>,
    idle_timeout_millis: u64,
    last_activity_at: DateTime<Utc>,
    max_output_bytes_per_stream: u64,
    max_cpu_seconds: u64,
    max_memory_bytes: Option<u64>,
    observed_cpu_usage_micros: u64,
    observed_stdout_bytes: u64,
    observed_stderr_bytes: u64,
    termination_reason: Option<ProcessSessionTerminationReason>,
    updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedLegacyProcessSessionManifestV3 {
    manifest: LegacyProcessSessionManifestV3,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LegacyProcessSessionManifestV2 {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    governance_digest: String,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: DateTime<Utc>,
    execution_deadline_at: DateTime<Utc>,
    idle_timeout_millis: u64,
    last_activity_at: DateTime<Utc>,
    max_output_bytes_per_stream: u64,
    max_cpu_seconds: u64,
    max_memory_bytes: Option<u64>,
    observed_stdout_bytes: u64,
    observed_stderr_bytes: u64,
    termination_reason: Option<ProcessSessionTerminationReason>,
    updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedLegacyProcessSessionManifestV2 {
    manifest: LegacyProcessSessionManifestV2,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LegacyProcessSessionManifest {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl LegacyProcessSessionManifest {
    fn is_well_formed_terminal(&self) -> bool {
        self.schema_version == 1
            && !self.session_id.is_nil()
            && !self.tenant_id.is_nil()
            && self.workspace_root.is_absolute()
            && !self.source_run_id.is_nil()
            && !self.source_attempt_id.is_nil()
            && !self.source_tool_call_id.trim().is_empty()
            && self.source_tool_call_id.len() <= 256
            && is_sha256(&self.source_binding_digest)
            && is_sha256(&self.implementation_digest)
            && self.operation_sequence > 0
            && !self.last_operation.trim().is_empty()
            && self
                .last_input_digest
                .as_ref()
                .is_none_or(|digest| is_sha256(digest))
            && self.updated_at >= self.started_at
            && self.state.is_terminal()
            && self.pid.is_none()
            && self.process_group_id.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedLegacyProcessSessionManifest {
    manifest: LegacyProcessSessionManifest,
    digest: String,
}

pub struct PersistentProcessSessionManager {
    state_root: PathBuf,
    executor: TrustedNativeExecutor,
    max_output_chunk_bytes: usize,
    governance: ProcessSessionGovernance,
    resource_capabilities: ProcessSessionResourceCapabilities,
    resource_backend: ProcessSessionResourceBackend,
    pty_supervisor: Option<ProcessSessionPtySupervisorConfig>,
    wait_observation_metrics: Arc<ProcessWaitObservationMetrics>,
    wait_observers: Mutex<HashMap<Uuid, Weak<ProcessWaitObserver>>>,
}

#[derive(Default)]
struct ProcessWaitObservationMetrics {
    active_waiters: AtomicUsize,
    active_observers: AtomicUsize,
    filesystem_observations: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessWaitObservationSnapshot {
    pub active_waiters: usize,
    pub active_observers: usize,
    pub filesystem_observations: u64,
}

struct ProcessWaiterGuard {
    metrics: Arc<ProcessWaitObservationMetrics>,
}

impl ProcessWaiterGuard {
    fn new(metrics: Arc<ProcessWaitObservationMetrics>) -> Self {
        metrics.active_waiters.fetch_add(1, Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for ProcessWaiterGuard {
    fn drop(&mut self) {
        self.metrics.active_waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessWaitObservation {
    Unknown,
    Available {
        state: ProcessSessionState,
        stdout_cursor: u64,
        stderr_cursor: u64,
    },
    Unavailable,
}

struct ProcessWaitObserver {
    sender: watch::Sender<ProcessWaitObservation>,
    wake: Notify,
}

struct ProcessWaitObserverGuard {
    metrics: Arc<ProcessWaitObservationMetrics>,
}

impl ProcessWaitObserverGuard {
    fn new(metrics: Arc<ProcessWaitObservationMetrics>) -> Self {
        metrics.active_observers.fetch_add(1, Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for ProcessWaitObserverGuard {
    fn drop(&mut self) {
        self.metrics
            .active_observers
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessTerminalSize {
    cols: u16,
    rows: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProcessTerminalMarker {
    schema_version: u32,
    session_id: Uuid,
    cols: u16,
    rows: u16,
    #[serde(default)]
    supervisor_id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedProcessTerminalMarker {
    marker: ProcessTerminalMarker,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProcessInteractionReceipt {
    schema_version: u32,
    operation: String,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    input_digest: String,
    stdout_cursor: u64,
    stderr_cursor: u64,
    committed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedProcessInteractionReceipt {
    receipt: ProcessInteractionReceipt,
    digest: String,
}

impl ProcessInteractionReceipt {
    fn is_well_formed(&self) -> bool {
        self.schema_version == 1
            && matches!(self.operation.as_str(), "write" | "close")
            && !self.session_id.is_nil()
            && !self.tenant_id.is_nil()
            && self.workspace_root.is_absolute()
            && !self.source_run_id.is_nil()
            && !self.source_attempt_id.is_nil()
            && !self.source_tool_call_id.trim().is_empty()
            && self.source_tool_call_id.len() <= 256
            && is_sha256(&self.source_binding_digest)
            && is_sha256(&self.input_digest)
    }
}

impl ProcessTerminalMarker {
    fn is_well_formed(&self) -> bool {
        matches!(self.schema_version, 1 | 2)
            && !self.session_id.is_nil()
            && (1..=2_000).contains(&self.cols)
            && (1..=2_000).contains(&self.rows)
            && match self.schema_version {
                1 => self.supervisor_id.is_none(),
                2 => self
                    .supervisor_id
                    .is_some_and(|supervisor_id| !supervisor_id.is_nil()),
                _ => false,
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSessionToolOperation {
    Start,
    Write,
    Resize,
    Poll,
    Attach,
    Wait,
    Interrupt,
    Close,
}

impl ProcessSessionToolOperation {
    #[must_use]
    pub fn tool_name(self) -> &'static str {
        match self {
            Self::Start => PROCESS_START_TOOL,
            Self::Write => PROCESS_WRITE_TOOL,
            Self::Resize => PROCESS_RESIZE_TOOL,
            Self::Poll => PROCESS_POLL_TOOL,
            Self::Attach => PROCESS_ATTACH_TOOL,
            Self::Wait => PROCESS_WAIT_TOOL,
            Self::Interrupt => PROCESS_INTERRUPT_TOOL,
            Self::Close => PROCESS_CLOSE_TOOL,
        }
    }

    fn expected_effect(self) -> agent_protocol::ToolEffect {
        match self {
            Self::Poll | Self::Attach | Self::Wait => agent_protocol::ToolEffect::Pure,
            Self::Resize => agent_protocol::ToolEffect::Idempotent,
            Self::Start | Self::Write | Self::Interrupt | Self::Close => {
                agent_protocol::ToolEffect::NonIdempotent
            }
        }
    }
}

#[derive(Clone)]
pub struct ProcessSessionToolExecutor {
    manager: Arc<PersistentProcessSessionManager>,
    operation: ProcessSessionToolOperation,
    implementation_digest: String,
}

impl ProcessSessionToolExecutor {
    #[must_use]
    pub fn new(
        manager: Arc<PersistentProcessSessionManager>,
        operation: ProcessSessionToolOperation,
    ) -> Self {
        let implementation_digest = sha256(
            serde_json::json!({
                "tool_implementation_version": PROCESS_SESSION_TOOL_IMPLEMENTATION_VERSION,
                "manifest_schema_version": PROCESS_SESSION_SCHEMA_VERSION,
                "manager_implementation_digest": manager.implementation_digest(),
                "governance_digest": manager.governance_digest(),
                "operation": operation.tool_name(),
            })
            .to_string()
            .as_bytes(),
        );
        Self {
            manager,
            operation,
            implementation_digest,
        }
    }

    fn validate_yield_time(
        yield_time_ms: u64,
        context: &ToolExecutionContext,
    ) -> Result<Duration, ToolExecutionError> {
        if yield_time_ms == 0 || yield_time_ms > 300_000 {
            return Err(ToolExecutionError::PersistentProcessSession(
                "yield-time_ms must be between 1 and 300000".into(),
            ));
        }
        let requested_wait = Duration::from_millis(yield_time_ms);
        if context.timeout.is_zero() || requested_wait > context.timeout {
            return Err(ToolExecutionError::PersistentProcessSession(
                "process yield exceeds the frozen Tool execution timeout".into(),
            ));
        }
        Ok(requested_wait)
    }

    async fn wait_for_relevant_output(
        &self,
        access: &ProcessSessionAccess,
        interaction: &ProcessSessionInteraction,
        request: ProcessYieldRequest,
        context: &ToolExecutionContext,
    ) -> Result<ProcessSessionOutput, ToolExecutionError> {
        let mut output = match request.initial_output {
            Some(output) => output,
            None => {
                self.manager
                    .wait_observation_metrics
                    .filesystem_observations
                    .fetch_add(1, Ordering::Relaxed);
                self.manager
                    .interact(access, interaction.clone())
                    .await
                    .map_err(process_session_tool_error)?
            }
        };
        if process_output_is_relevant(&output, request.stdout_cursor, request.stderr_cursor) {
            return Ok(output);
        }

        let mut observations = self
            .manager
            .subscribe_wait_observation(interaction.session_id);
        let _waiter = ProcessWaiterGuard::new(Arc::clone(&self.manager.wait_observation_metrics));
        let requested_deadline = tokio::time::Instant::now() + request.requested_wait;
        let execution_deadline = request.execution_started_at + context.timeout;
        let deadline = requested_deadline.min(execution_deadline);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break Ok(output);
            }
            tokio::select! {
                () = context.cancellation.cancelled() => {
                    return Err(ToolExecutionError::Cancelled);
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.manager
                        .wait_observation_metrics
                        .filesystem_observations
                        .fetch_add(1, Ordering::Relaxed);
                    output = self.manager
                        .interact(access, interaction.clone())
                        .await
                        .map_err(process_session_tool_error)?;
                    break Ok(output);
                }
                changed = observations.changed() => {
                    if changed.is_err() {
                        observations = self
                            .manager
                            .subscribe_wait_observation(interaction.session_id);
                        continue;
                    }
                    let observation = *observations.borrow_and_update();
                    let relevant = match observation {
                        ProcessWaitObservation::Unknown => false,
                        ProcessWaitObservation::Available {
                            state,
                            stdout_cursor: observed_stdout_cursor,
                            stderr_cursor: observed_stderr_cursor,
                        } => {
                            state.is_terminal()
                                || observed_stdout_cursor > request.stdout_cursor
                                || observed_stderr_cursor > request.stderr_cursor
                        }
                        ProcessWaitObservation::Unavailable => true,
                    };
                    if !relevant {
                        continue;
                    }
                    self.manager
                        .wait_observation_metrics
                        .filesystem_observations
                        .fetch_add(1, Ordering::Relaxed);
                    let refreshed = match observation {
                        ProcessWaitObservation::Available { .. } => self
                            .manager
                            .read_observed_wait_output(access, interaction),
                        ProcessWaitObservation::Unknown
                        | ProcessWaitObservation::Unavailable => self
                            .manager
                            .interact(access, interaction.clone())
                            .await,
                    };
                    output = refreshed.map_err(process_session_tool_error)?;
                    if process_output_is_relevant(
                        &output,
                        request.stdout_cursor,
                        request.stderr_cursor,
                    ) {
                        break Ok(output);
                    }
                }
            }
        }
    }

    async fn execute_operation(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        let execution_started_at = tokio::time::Instant::now();
        if request.call.name != self.operation.tool_name()
            || request.effect != self.operation.expected_effect()
        {
            return Err(ToolExecutionError::PersistentProcessSession(
                "Tool name or effect does not match the registered process operation".into(),
            ));
        }
        if request.sandbox != agent_protocol::SandboxClass::TrustedNative {
            return Err(ToolExecutionError::WrongSandbox);
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolExecutionError::Cancelled);
        }
        let output: ProcessSessionOutput = match self.operation {
            ProcessSessionToolOperation::Start => {
                let arguments: ProcessStartArguments =
                    serde_json::from_value(request.call.arguments.clone()).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                let requested_wait = arguments
                    .yield_time_ms
                    .map(|yield_time_ms| Self::validate_yield_time(yield_time_ms, &context))
                    .transpose()?;
                let access = ProcessSessionAccess {
                    tenant_id: context.tenant_id,
                    workspace_root: context.workspace_root.clone(),
                };
                let start = ProcessSessionStartRequest {
                    session_id: Uuid::now_v7(),
                    request,
                    context: context.clone(),
                    initial_stdin: arguments.initial_stdin.into_bytes(),
                };
                let output = if arguments.tty {
                    self.manager
                        .start_pty(start, arguments.cols, arguments.rows)
                        .await
                } else {
                    self.manager.start(start).await
                }
                .map_err(process_session_tool_error)?;
                if let Some(requested_wait) = requested_wait {
                    self.wait_for_relevant_output(
                        &access,
                        &ProcessSessionInteraction {
                            session_id: output.session_id,
                            stdout_cursor: 0,
                            stderr_cursor: 0,
                            action: ProcessSessionAction::Poll,
                        },
                        ProcessYieldRequest {
                            initial_output: Some(output),
                            stdout_cursor: 0,
                            stderr_cursor: 0,
                            requested_wait,
                            execution_started_at,
                        },
                        &context,
                    )
                    .await
                } else {
                    Ok(output)
                }
            }
            ProcessSessionToolOperation::Write => {
                let arguments: ProcessWriteArguments =
                    serde_json::from_value(request.call.arguments.clone()).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                let requested_wait = arguments
                    .yield_time_ms
                    .map(|yield_time_ms| Self::validate_yield_time(yield_time_ms, &context))
                    .transpose()?;
                let access = ProcessSessionAccess {
                    tenant_id: context.tenant_id,
                    workspace_root: context.workspace_root.clone(),
                };
                let stdin = arguments.stdin.as_bytes().to_vec();
                let output = self
                    .manager
                    .interact(
                        &access,
                        ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            action: ProcessSessionAction::Write {
                                bytes: stdin.clone(),
                            },
                        },
                    )
                    .await
                    .map_err(process_session_tool_error)?;
                self.manager
                    .persist_committed_write_receipt(&request, &context, &arguments)
                    .map_err(process_session_tool_error)?;
                if let Some(requested_wait) = requested_wait {
                    self.wait_for_relevant_output(
                        &access,
                        &ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            action: ProcessSessionAction::Poll,
                        },
                        ProcessYieldRequest {
                            initial_output: Some(output),
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            requested_wait,
                            execution_started_at,
                        },
                        &context,
                    )
                    .await
                } else {
                    Ok(output)
                }
            }
            ProcessSessionToolOperation::Resize => {
                let arguments: ProcessResizeArguments =
                    serde_json::from_value(request.call.arguments).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                self.manager
                    .interact(
                        &ProcessSessionAccess {
                            tenant_id: context.tenant_id,
                            workspace_root: context.workspace_root,
                        },
                        ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            action: ProcessSessionAction::Resize {
                                cols: arguments.cols,
                                rows: arguments.rows,
                            },
                        },
                    )
                    .await
                    .map_err(process_session_tool_error)
            }
            ProcessSessionToolOperation::Attach => {
                let arguments: ProcessAttachArguments =
                    serde_json::from_value(request.call.arguments).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                self.manager
                    .interact(
                        &ProcessSessionAccess {
                            tenant_id: context.tenant_id,
                            workspace_root: context.workspace_root,
                        },
                        ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: 0,
                            stderr_cursor: 0,
                            action: ProcessSessionAction::Attach {
                                max_bytes: arguments.max_bytes,
                            },
                        },
                    )
                    .await
                    .map_err(process_session_tool_error)
            }
            ProcessSessionToolOperation::Wait => {
                let arguments: ProcessWaitArguments =
                    serde_json::from_value(request.call.arguments).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                let requested_wait = Self::validate_yield_time(arguments.yield_time_ms, &context)?;
                let access = ProcessSessionAccess {
                    tenant_id: context.tenant_id,
                    workspace_root: context.workspace_root.clone(),
                };
                let interaction = ProcessSessionInteraction {
                    session_id: arguments.session_id,
                    stdout_cursor: arguments.stdout_cursor,
                    stderr_cursor: arguments.stderr_cursor,
                    action: ProcessSessionAction::Poll,
                };
                self.wait_for_relevant_output(
                    &access,
                    &interaction,
                    ProcessYieldRequest {
                        initial_output: None,
                        stdout_cursor: arguments.stdout_cursor,
                        stderr_cursor: arguments.stderr_cursor,
                        requested_wait,
                        execution_started_at,
                    },
                    &context,
                )
                .await
            }
            ProcessSessionToolOperation::Close => {
                let arguments: ProcessInteractArguments =
                    serde_json::from_value(request.call.arguments.clone()).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                self.manager
                    .persist_close_intent(&request, &context, &arguments)
                    .map_err(process_session_tool_error)?;
                self.manager
                    .interact(
                        &ProcessSessionAccess {
                            tenant_id: context.tenant_id,
                            workspace_root: context.workspace_root,
                        },
                        ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            action: ProcessSessionAction::Close,
                        },
                    )
                    .await
                    .map_err(process_session_tool_error)
            }
            operation => {
                let arguments: ProcessInteractArguments =
                    serde_json::from_value(request.call.arguments).map_err(|error| {
                        ToolExecutionError::PersistentProcessSession(error.to_string())
                    })?;
                let action = match operation {
                    ProcessSessionToolOperation::Poll => ProcessSessionAction::Poll,
                    ProcessSessionToolOperation::Interrupt => ProcessSessionAction::Interrupt,
                    ProcessSessionToolOperation::Close => unreachable!("handled above"),
                    ProcessSessionToolOperation::Start
                    | ProcessSessionToolOperation::Write
                    | ProcessSessionToolOperation::Resize
                    | ProcessSessionToolOperation::Attach
                    | ProcessSessionToolOperation::Wait => {
                        unreachable!("handled above")
                    }
                };
                self.manager
                    .interact(
                        &ProcessSessionAccess {
                            tenant_id: context.tenant_id,
                            workspace_root: context.workspace_root,
                        },
                        ProcessSessionInteraction {
                            session_id: arguments.session_id,
                            stdout_cursor: arguments.stdout_cursor,
                            stderr_cursor: arguments.stderr_cursor,
                            action,
                        },
                    )
                    .await
                    .map_err(process_session_tool_error)
            }
        }?;
        process_output_tool_result(output)
    }
}

fn process_output_tool_result(
    output: ProcessSessionOutput,
) -> Result<ToolExecutionResult, ToolExecutionError> {
    Ok(ToolExecutionResult {
        content: serde_json::to_value(output)
            .map_err(|error| ToolExecutionError::PersistentProcessSession(error.to_string()))?,
        is_error: false,
        exit_code: 0,
    })
}

fn process_output_is_relevant(
    output: &ProcessSessionOutput,
    stdout_cursor: u64,
    stderr_cursor: u64,
) -> bool {
    output.state.is_terminal()
        || output.stdout_cursor > stdout_cursor
        || output.stderr_cursor > stderr_cursor
}

fn process_session_tool_error(error: ProcessSessionError) -> ToolExecutionError {
    match error {
        ProcessSessionError::StartFailed { session_id, reason } => {
            ToolExecutionError::ProcessSessionStartFailed { session_id, reason }
        }
        error => ToolExecutionError::PersistentProcessSession(error.to_string()),
    }
}

impl ToolExecutor for ProcessSessionToolExecutor {
    fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ToolExecutionResult, ToolExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(self.execute_operation(request, context))
    }

    fn recover_started_result(
        &self,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<ToolExecutionResult>, ToolExecutionError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            if !matches!(
                self.operation,
                ProcessSessionToolOperation::Start
                    | ProcessSessionToolOperation::Write
                    | ProcessSessionToolOperation::Close
            ) || request.call.name != self.operation.tool_name()
                || request.effect != agent_protocol::ToolEffect::NonIdempotent
            {
                return Ok(None);
            }
            let output = match self.operation {
                ProcessSessionToolOperation::Start => self
                    .manager
                    .recover_started_session(&request, &context)
                    .await
                    .map_err(process_session_tool_error)?,
                ProcessSessionToolOperation::Write => {
                    let arguments: ProcessWriteArguments =
                        serde_json::from_value(request.call.arguments.clone()).map_err(
                            |error| ToolExecutionError::PersistentProcessSession(error.to_string()),
                        )?;
                    self.manager
                        .recover_committed_write(&request, &context, &arguments)
                        .await
                        .map_err(process_session_tool_error)?
                }
                ProcessSessionToolOperation::Close => {
                    let arguments: ProcessInteractArguments =
                        serde_json::from_value(request.call.arguments.clone()).map_err(
                            |error| ToolExecutionError::PersistentProcessSession(error.to_string()),
                        )?;
                    self.manager
                        .recover_close_intent(&request, &context, &arguments)
                        .await
                        .map_err(process_session_tool_error)?
                }
                _ => None,
            };
            output.map(process_output_tool_result).transpose()
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessStartArguments {
    #[serde(default)]
    initial_stdin: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    tty: bool,
    #[serde(default = "default_terminal_cols")]
    cols: u16,
    #[serde(default = "default_terminal_rows")]
    rows: u16,
}

const fn default_terminal_cols() -> u16 {
    80
}

const fn default_terminal_rows() -> u16 {
    24
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessWriteArguments {
    session_id: Uuid,
    #[serde(default)]
    stdout_cursor: u64,
    #[serde(default)]
    stderr_cursor: u64,
    stdin: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessInteractArguments {
    session_id: Uuid,
    #[serde(default)]
    stdout_cursor: u64,
    #[serde(default)]
    stderr_cursor: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessResizeArguments {
    session_id: Uuid,
    #[serde(default)]
    stdout_cursor: u64,
    #[serde(default)]
    stderr_cursor: u64,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessAttachArguments {
    session_id: Uuid,
    max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessWaitArguments {
    session_id: Uuid,
    #[serde(default)]
    stdout_cursor: u64,
    #[serde(default)]
    stderr_cursor: u64,
    yield_time_ms: u64,
}

impl PersistentProcessSessionManager {
    pub fn new(
        state_root: PathBuf,
        executor: TrustedNativeExecutor,
        max_output_chunk_bytes: usize,
    ) -> Result<Self, ProcessSessionError> {
        Self::new_with_governance(
            state_root,
            executor,
            max_output_chunk_bytes,
            ProcessSessionGovernance::default(),
        )
    }

    pub fn new_with_governance(
        state_root: PathBuf,
        executor: TrustedNativeExecutor,
        max_output_chunk_bytes: usize,
        governance: ProcessSessionGovernance,
    ) -> Result<Self, ProcessSessionError> {
        Self::new_with_governance_and_pty_supervisor(
            state_root,
            executor,
            max_output_chunk_bytes,
            governance,
            None,
        )
    }

    pub fn new_with_governance_and_pty_supervisor(
        state_root: PathBuf,
        executor: TrustedNativeExecutor,
        max_output_chunk_bytes: usize,
        governance: ProcessSessionGovernance,
        pty_supervisor: Option<ProcessSessionPtySupervisorConfig>,
    ) -> Result<Self, ProcessSessionError> {
        if max_output_chunk_bytes == 0 || max_output_chunk_bytes > MAX_OUTPUT_CHUNK_BYTES {
            return Err(ProcessSessionError::InvalidConfiguration(
                "output chunks must be between 1 byte and 1 MiB".into(),
            ));
        }
        let resource_capabilities = resolve_resource_capabilities(&governance.resource_backend)?;
        validate_governance(&governance, max_output_chunk_bytes, resource_capabilities)?;
        let resource_backend = ProcessSessionResourceBackend::open(&governance.resource_backend)?;
        std::fs::create_dir_all(&state_root)
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
        let state_root = std::fs::canonicalize(state_root)
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
        if !state_root.is_dir() {
            return Err(ProcessSessionError::InvalidConfiguration(
                "state root must be a directory".into(),
            ));
        }
        std::fs::create_dir_all(state_root.join("process-sessions"))
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
        ensure_owner_only_directory(&state_root.join("process-sessions"))?;
        if let Some(supervisor) = &pty_supervisor {
            pty_supervisor::validate_config(supervisor)?;
            pty_supervisor::ensure_control_token(&state_root)?;
        }
        Ok(Self {
            state_root,
            executor,
            max_output_chunk_bytes,
            governance,
            resource_capabilities,
            resource_backend,
            pty_supervisor,
            wait_observation_metrics: Arc::new(ProcessWaitObservationMetrics::default()),
            wait_observers: Mutex::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn implementation_digest(&self) -> &str {
        self.executor.implementation_digest()
    }

    #[must_use]
    pub fn governance_digest(&self) -> String {
        governance_digest(&self.governance, self.resource_capabilities)
    }

    #[must_use]
    pub const fn resource_capabilities(&self) -> ProcessSessionResourceCapabilities {
        self.resource_capabilities
    }

    async fn recover_started_session(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
    ) -> Result<Option<ProcessSessionOutput>, ProcessSessionError> {
        let workspace_root = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
        let mut matched = None;
        for entry in std::fs::read_dir(self.sessions_root())
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
        {
            let Ok(manifest) = load_manifest(&entry.path()) else {
                continue;
            };
            if manifest.tenant_id != context.tenant_id
                || manifest.workspace_root != workspace_root
                || manifest.source_run_id != context.run_id
                || manifest.source_attempt_id != context.attempt_id
                || manifest.source_tool_call_id != request.call.id
                || manifest.source_binding_digest != request.binding_digest
            {
                continue;
            }
            if matched.replace(manifest.session_id).is_some() {
                return Err(ProcessSessionError::Indeterminate);
            }
        }
        let Some(session_id) = matched else {
            return Ok(None);
        };
        let session_dir = self.session_dir(session_id);
        let _ =
            sweep_process_session_retrying_conflicts(&session_dir, &self.resource_backend).await?;
        let manifest = load_manifest(&session_dir)?;
        self.validate_access(
            &ProcessSessionAccess {
                tenant_id: context.tenant_id,
                workspace_root: workspace_root.clone(),
            },
            &manifest,
        )?;
        if manifest.source_run_id != context.run_id
            || manifest.source_attempt_id != context.attempt_id
            || manifest.source_tool_call_id != request.call.id
            || manifest.source_binding_digest != request.binding_digest
            || manifest.state == ProcessSessionState::Starting
            || manifest.state == ProcessSessionState::Indeterminate
            || manifest.last_operation == "start_failed"
        {
            return Ok(None);
        }
        self.output_from_manifest(&manifest, 0, 0).map(Some)
    }

    async fn recover_committed_write(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
        arguments: &ProcessWriteArguments,
    ) -> Result<Option<ProcessSessionOutput>, ProcessSessionError> {
        let session_dir = self.session_dir(arguments.session_id);
        let _ =
            sweep_process_session_retrying_conflicts(&session_dir, &self.resource_backend).await?;
        let manifest = load_manifest(&session_dir)?;
        let workspace_root = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
        self.validate_access(
            &ProcessSessionAccess {
                tenant_id: context.tenant_id,
                workspace_root: workspace_root.clone(),
            },
            &manifest,
        )?;
        let receipt = load_interaction_receipt(&session_dir, &request.binding_digest)?;
        if receipt.operation != "write"
            || receipt.session_id != arguments.session_id
            || receipt.tenant_id != context.tenant_id
            || receipt.workspace_root != workspace_root
            || receipt.source_run_id != context.run_id
            || receipt.source_attempt_id != context.attempt_id
            || receipt.source_tool_call_id != request.call.id
            || receipt.source_binding_digest != request.binding_digest
            || receipt.input_digest != sha256(arguments.stdin.as_bytes())
            || receipt.stdout_cursor != arguments.stdout_cursor
            || receipt.stderr_cursor != arguments.stderr_cursor
            || manifest.state == ProcessSessionState::Starting
            || manifest.state == ProcessSessionState::Indeterminate
        {
            return Ok(None);
        }
        self.output_from_manifest(&manifest, arguments.stdout_cursor, arguments.stderr_cursor)
            .map(Some)
    }

    async fn recover_close_intent(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
        arguments: &ProcessInteractArguments,
    ) -> Result<Option<ProcessSessionOutput>, ProcessSessionError> {
        let session_dir = self.session_dir(arguments.session_id);
        let manifest = load_manifest(&session_dir)?;
        let workspace_root = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
        let access = ProcessSessionAccess {
            tenant_id: context.tenant_id,
            workspace_root: workspace_root.clone(),
        };
        self.validate_access(&access, &manifest)?;
        let receipt = load_interaction_receipt(&session_dir, &request.binding_digest)?;
        let arguments_digest = sha256(
            &serde_json::to_vec(&request.call.arguments)
                .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?,
        );
        if receipt.operation != "close"
            || receipt.session_id != arguments.session_id
            || receipt.tenant_id != context.tenant_id
            || receipt.workspace_root != workspace_root
            || receipt.source_run_id != context.run_id
            || receipt.source_attempt_id != context.attempt_id
            || receipt.source_tool_call_id != request.call.id
            || receipt.source_binding_digest != request.binding_digest
            || receipt.input_digest != arguments_digest
            || receipt.stdout_cursor != arguments.stdout_cursor
            || receipt.stderr_cursor != arguments.stderr_cursor
            || manifest.state == ProcessSessionState::Starting
            || manifest.state == ProcessSessionState::Indeterminate
            || (manifest.state == ProcessSessionState::Terminating
                && manifest.termination_reason != Some(ProcessSessionTerminationReason::Closed))
        {
            return Ok(None);
        }
        let output = self
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id: arguments.session_id,
                    stdout_cursor: arguments.stdout_cursor,
                    stderr_cursor: arguments.stderr_cursor,
                    action: ProcessSessionAction::Close,
                },
            )
            .await?;
        if output.state == ProcessSessionState::Terminated
            && output.termination_reason == Some(ProcessSessionTerminationReason::Closed)
        {
            Ok(Some(output))
        } else {
            Ok(None)
        }
    }

    fn persist_committed_write_receipt(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
        arguments: &ProcessWriteArguments,
    ) -> Result<(), ProcessSessionError> {
        let session_dir = self.session_dir(arguments.session_id);
        let manifest = load_manifest(&session_dir)?;
        let workspace_root = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
        self.validate_access(
            &ProcessSessionAccess {
                tenant_id: context.tenant_id,
                workspace_root: workspace_root.clone(),
            },
            &manifest,
        )?;
        persist_interaction_receipt(
            &session_dir,
            &ProcessInteractionReceipt {
                schema_version: 1,
                operation: "write".into(),
                session_id: arguments.session_id,
                tenant_id: context.tenant_id,
                workspace_root,
                source_run_id: context.run_id,
                source_attempt_id: context.attempt_id,
                source_tool_call_id: request.call.id.clone(),
                source_binding_digest: request.binding_digest.clone(),
                input_digest: sha256(arguments.stdin.as_bytes()),
                stdout_cursor: arguments.stdout_cursor,
                stderr_cursor: arguments.stderr_cursor,
                committed_at: Utc::now(),
            },
        )
    }

    fn persist_close_intent(
        &self,
        request: &ToolExecutionRequest,
        context: &ToolExecutionContext,
        arguments: &ProcessInteractArguments,
    ) -> Result<(), ProcessSessionError> {
        let session_dir = self.session_dir(arguments.session_id);
        let manifest = load_manifest(&session_dir)?;
        let workspace_root = std::fs::canonicalize(&context.workspace_root)
            .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
        self.validate_access(
            &ProcessSessionAccess {
                tenant_id: context.tenant_id,
                workspace_root: workspace_root.clone(),
            },
            &manifest,
        )?;
        if manifest.state.is_terminal() {
            // A close observed after natural exit has no remaining external
            // side effect to reconcile. Preserve the existing terminal result
            // without manufacturing a close intent receipt.
            return Ok(());
        }
        if manifest.state != ProcessSessionState::Running {
            return Err(ProcessSessionError::Conflict);
        }
        let arguments_digest = sha256(
            &serde_json::to_vec(&request.call.arguments)
                .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?,
        );
        persist_interaction_receipt(
            &session_dir,
            &ProcessInteractionReceipt {
                schema_version: 1,
                operation: "close".into(),
                session_id: arguments.session_id,
                tenant_id: context.tenant_id,
                workspace_root,
                source_run_id: context.run_id,
                source_attempt_id: context.attempt_id,
                source_tool_call_id: request.call.id.clone(),
                source_binding_digest: request.binding_digest.clone(),
                input_digest: arguments_digest,
                stdout_cursor: arguments.stdout_cursor,
                stderr_cursor: arguments.stderr_cursor,
                committed_at: Utc::now(),
            },
        )
    }

    #[must_use]
    pub fn wait_observation_snapshot(&self) -> ProcessWaitObservationSnapshot {
        ProcessWaitObservationSnapshot {
            active_waiters: self
                .wait_observation_metrics
                .active_waiters
                .load(Ordering::Relaxed),
            active_observers: self
                .wait_observation_metrics
                .active_observers
                .load(Ordering::Relaxed),
            filesystem_observations: self
                .wait_observation_metrics
                .filesystem_observations
                .load(Ordering::Relaxed),
        }
    }

    fn subscribe_wait_observation(
        self: &Arc<Self>,
        session_id: Uuid,
    ) -> watch::Receiver<ProcessWaitObservation> {
        let mut observers = self
            .wait_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(observer) = observers.get(&session_id).and_then(Weak::upgrade) {
            return observer.sender.subscribe();
        }

        let (sender, receiver) = watch::channel(ProcessWaitObservation::Unknown);
        let observer = Arc::new(ProcessWaitObserver {
            sender,
            wake: Notify::new(),
        });
        observers.insert(session_id, Arc::downgrade(&observer));
        let manager = Arc::clone(self);
        let task_observer = Arc::clone(&observer);
        tokio::spawn(async move {
            manager
                .run_process_wait_observer(session_id, task_observer)
                .await;
        });
        receiver
    }

    async fn run_process_wait_observer(
        self: Arc<Self>,
        session_id: Uuid,
        observer: Arc<ProcessWaitObserver>,
    ) {
        let _observer_guard =
            ProcessWaitObserverGuard::new(Arc::clone(&self.wait_observation_metrics));
        let mut previous = ProcessWaitObservation::Unknown;
        loop {
            tokio::select! {
                () = tokio::time::sleep(PROCESS_WAIT_OBSERVATION_INTERVAL) => {}
                () = observer.wake.notified() => {}
            }
            if self.retire_wait_observer_if_idle(session_id, &observer) {
                break;
            }

            self.wait_observation_metrics
                .filesystem_observations
                .fetch_add(1, Ordering::Relaxed);
            let observation = self
                .observe_process_wait_state(session_id)
                .await
                .unwrap_or(ProcessWaitObservation::Unavailable);
            if observation != previous {
                observer.sender.send_replace(observation);
                previous = observation;
            }
        }
    }

    fn wake_wait_observer(&self, session_id: Uuid) {
        let observer = self
            .wait_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .and_then(Weak::upgrade);
        if let Some(observer) = observer {
            observer.wake.notify_one();
        }
    }

    fn retire_wait_observer_if_idle(
        &self,
        session_id: Uuid,
        observer: &Arc<ProcessWaitObserver>,
    ) -> bool {
        if observer.sender.receiver_count() != 0 {
            return false;
        }
        let mut observers = self
            .wait_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if observer.sender.receiver_count() != 0 {
            return false;
        }
        let is_current = observers
            .get(&session_id)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, observer));
        if is_current {
            observers.remove(&session_id);
        }
        true
    }

    async fn observe_process_wait_state(
        &self,
        session_id: Uuid,
    ) -> Result<ProcessWaitObservation, ProcessSessionError> {
        let session_dir = self.session_dir(session_id);
        let _ =
            sweep_process_session_retrying_conflicts(&session_dir, &self.resource_backend).await?;
        let manifest = load_manifest(&session_dir)?;
        let stdout_cursor = std::fs::metadata(session_dir.join("stdout.log"))
            .map_err(io_error)?
            .len();
        let stderr_cursor = std::fs::metadata(session_dir.join("stderr.log"))
            .map_err(io_error)?
            .len();
        Ok(ProcessWaitObservation::Available {
            state: manifest.state,
            stdout_cursor,
            stderr_cursor,
        })
    }

    fn read_observed_wait_output(
        &self,
        access: &ProcessSessionAccess,
        interaction: &ProcessSessionInteraction,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        if !matches!(interaction.action, ProcessSessionAction::Poll) {
            return Err(ProcessSessionError::InvalidRequest(
                "observed process wait only supports poll reads".into(),
            ));
        }
        let session_dir = self.session_dir(interaction.session_id);
        let manifest = load_manifest(&session_dir)?;
        self.validate_access(access, &manifest)?;
        self.output_from_manifest(
            &manifest,
            interaction.stdout_cursor,
            interaction.stderr_cursor,
        )
    }

    pub async fn start(
        &self,
        request: ProcessSessionStartRequest,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        self.start_with_terminal(request, None).await
    }

    pub async fn start_pty(
        &self,
        request: ProcessSessionStartRequest,
        cols: u16,
        rows: u16,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        if self.pty_supervisor.is_none() {
            return Err(ProcessSessionError::InvalidConfiguration(
                "resumable PTY sessions require an external supervisor".into(),
            ));
        }
        if cols == 0 || rows == 0 || cols > 2_000 || rows > 2_000 {
            return Err(ProcessSessionError::InvalidRequest(
                "terminal dimensions must be between 1 and 2000 cells".into(),
            ));
        }
        self.start_with_terminal(request, Some(ProcessTerminalSize { cols, rows }))
            .await
    }

    async fn start_with_terminal(
        &self,
        request: ProcessSessionStartRequest,
        terminal_size: Option<ProcessTerminalSize>,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        #[cfg(not(unix))]
        {
            let _ = request;
            return Err(ProcessSessionError::UnsupportedPlatform);
        }
        #[cfg(unix)]
        {
            self.validate_start(&request)?;
            let launch = self
                .executor
                .prepare(&request.request, &request.context)
                .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
            let workspace_root = std::fs::canonicalize(&request.context.workspace_root)
                .map_err(|error| ProcessSessionError::InvalidRequest(error.to_string()))?;
            // A process-wide count is insufficient because replacement Hosts
            // may start concurrently. The lock covers count, reservation and
            // publication of the Running identity.
            let capacity_lock = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(self.state_root.join("process-sessions.capacity.lock"))
                .map_err(io_error)?;
            lock_exclusive(&capacity_lock)?;
            let session_dir = self.session_dir(request.session_id);
            self.enforce_admission_quotas(request.context.tenant_id, &workspace_root)?;
            match std::fs::create_dir(&session_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(ProcessSessionError::Conflict);
                }
                Err(error) => return Err(ProcessSessionError::Io(error.to_string())),
            }
            ensure_owner_only_directory(&session_dir)?;

            let now = Utc::now();
            let execution_deadline_at = now
                .checked_add_signed(
                    chrono::Duration::from_std(self.governance.max_runtime).map_err(|_| {
                        ProcessSessionError::InvalidConfiguration(
                            "process max runtime cannot be represented".into(),
                        )
                    })?,
                )
                .ok_or_else(|| {
                    ProcessSessionError::InvalidConfiguration(
                        "process execution deadline overflows".into(),
                    )
                })?;
            let mut manifest = ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id: request.session_id,
                tenant_id: request.context.tenant_id,
                workspace_root,
                source_run_id: request.context.run_id,
                source_attempt_id: request.context.attempt_id,
                source_tool_call_id: request.request.call.id.clone(),
                source_binding_digest: request.request.binding_digest.clone(),
                implementation_digest: self.executor.implementation_digest().to_owned(),
                governance_digest: self.governance_digest(),
                resource_identity: ProcessSessionResourceIdentity::for_backend(
                    &self.resource_backend,
                    request.session_id,
                ),
                resource_phase: ProcessSessionResourcePhase::Unprepared,
                state: ProcessSessionState::Starting,
                pid: None,
                process_group_id: None,
                exit_code: None,
                operation_sequence: 1,
                last_operation: "start_intent".into(),
                last_input_digest: (!request.initial_stdin.is_empty())
                    .then(|| sha256(&request.initial_stdin)),
                recovery_count: 0,
                started_at: now,
                execution_deadline_at,
                idle_timeout_millis: u64::try_from(self.governance.idle_timeout.as_millis())
                    .map_err(|_| {
                        ProcessSessionError::InvalidConfiguration(
                            "process idle timeout cannot be represented".into(),
                        )
                    })?,
                last_activity_at: now,
                max_output_bytes_per_stream: self.governance.max_output_bytes_per_stream,
                max_cpu_seconds: self.governance.max_cpu_seconds,
                max_memory_bytes: self.governance.max_memory_bytes,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            };
            self.initialize_session_files(&session_dir)?;
            persist_manifest(&session_dir, &manifest)?;
            if let Some(size) = terminal_size {
                let supervisor = self.pty_supervisor.as_ref().ok_or_else(|| {
                    ProcessSessionError::InvalidConfiguration(
                        "resumable PTY sessions require an external supervisor".into(),
                    )
                })?;
                let start_result = pty_supervisor::start(
                    &self.state_root,
                    supervisor,
                    pty_supervisor::PtySupervisorStartRequest {
                        session_id: request.session_id,
                        launch,
                        initial_stdin: request.initial_stdin,
                        size,
                        max_output_chunk_bytes: self.max_output_chunk_bytes,
                        governance: self.governance.clone(),
                    },
                )
                .await;
                drop(capacity_lock);
                match start_result {
                    Ok((_supervisor_id, _pid)) => {
                        let running = load_manifest(&session_dir)?;
                        return self.output_from_manifest(&running, 0, 0);
                    }
                    Err(error) => {
                        if load_manifest(&session_dir)
                            .is_ok_and(|current| current.state == ProcessSessionState::Starting)
                            && finalize_start_failure(&session_dir, &manifest).is_err()
                        {
                            return Err(ProcessSessionError::Indeterminate);
                        }
                        return Err(error);
                    }
                }
            }
            let mut command = Command::new(&launch.program);
            command
                .args(&launch.args)
                .env_clear()
                .envs(launch.env.iter().cloned())
                .current_dir(&launch.current_dir)
                .kill_on_drop(false);
            let fifo_stdin = open_fifo_read_write(&session_dir.join("stdin.fifo"))?;
            command
                .stdin(Stdio::from(fifo_stdin.try_clone().map_err(io_error)?))
                .stdout(Stdio::from(open_append(&session_dir.join("stdout.log"))?))
                .stderr(Stdio::from(open_append(&session_dir.join("stderr.log"))?));
            command.process_group(0);
            install_identity_lease(&mut command, &session_dir.join("identity.lock"))?;
            install_process_resource_limits(
                &mut command,
                self.governance.max_output_bytes_per_stream,
                self.governance.max_cpu_seconds,
                self.governance.max_memory_bytes,
            )?;
            let mut prepared_linux_group =
                match (&self.resource_backend, &manifest.resource_identity) {
                    (
                        ProcessSessionResourceBackend::LinuxCgroupV2 { root },
                        ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
                    ) => Some(
                        prepare_linux_cgroup_v2_root(
                            root,
                            group_name,
                            self.governance.max_memory_bytes,
                            self.governance.max_processes,
                        )
                        .map_err(|error| {
                            ProcessSessionError::InvalidConfiguration(error.to_string())
                        })?,
                    ),
                    (
                        ProcessSessionResourceBackend::UnixRlimit,
                        ProcessSessionResourceIdentity::UnixRlimit,
                    ) => None,
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
            if let Err(error) = persist_manifest(&session_dir, &manifest) {
                drop(prepared_linux_group.take());
                remove_linux_cgroup_identity(
                    manifest.session_id,
                    &manifest.resource_identity,
                    &self.resource_backend,
                )?;
                return Err(error);
            }
            if let Some(group) = &prepared_linux_group
                && let Err(error) = install_linux_cgroup_membership_group(&mut command, group)
            {
                drop(prepared_linux_group.take());
                remove_linux_cgroup_identity(
                    manifest.session_id,
                    &manifest.resource_identity,
                    &self.resource_backend,
                )?;
                return Err(ProcessSessionError::InvalidConfiguration(error.to_string()));
            }
            // `pre_exec` runs in the post-fork child of a multi-threaded Host.
            // Serializing only this short spawn boundary avoids overlapping
            // fork/pre-exec setup across independent state roots; live process
            // execution remains fully concurrent.
            let spawn_guard = PROCESS_SESSION_SPAWN_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .map_err(|_| {
                    ProcessSessionError::Io("process-session spawn lock is poisoned".into())
                })?;
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    drop(spawn_guard);
                    drop(prepared_linux_group.take());
                    let reason = error.to_string();
                    finalize_start_failure(&session_dir, &manifest)?;
                    let terminal = load_manifest(&session_dir)?;
                    // A Linux cleanup failure intentionally leaves the durable
                    // terminal record at `cleanup_pending`; a replacement
                    // Manager retries it without changing start-failure truth.
                    let _cleanup_pending = cleanup_terminal_resource_identity(
                        &session_dir,
                        &terminal,
                        &self.resource_backend,
                    );
                    return Err(ProcessSessionError::StartFailed {
                        session_id: manifest.session_id,
                        reason,
                    });
                }
            };
            drop(spawn_guard);
            drop(prepared_linux_group);
            let pid = child
                .id()
                .ok_or_else(|| ProcessSessionError::Io("spawned process has no pid".into()))?;
            manifest.state = ProcessSessionState::Running;
            manifest.resource_phase = ProcessSessionResourcePhase::Active;
            manifest.pid = Some(pid);
            manifest.process_group_id = i32::try_from(pid).ok();
            manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
            manifest.last_operation = "started".into();
            manifest.updated_at = Utc::now();
            if !manifest.is_well_formed() {
                let _ = signal_group(i32::try_from(pid).unwrap_or(i32::MAX), libc::SIGKILL);
                return Err(ProcessSessionError::InvalidRequest(
                    "spawned process identity cannot be represented".into(),
                ));
            }
            persist_manifest(&session_dir, &manifest)?;
            if !request.initial_stdin.is_empty() {
                write_fifo(&session_dir.join("stdin.fifo"), &request.initial_stdin)?;
            }
            drop(fifo_stdin);
            // Closing the file releases the cross-process flock only after the
            // new live identity is durable and visible to the next starter.
            drop(capacity_lock);

            let watched_dir = session_dir.clone();
            let watched_manifest = manifest.clone();
            let watched_resource_backend = self.resource_backend.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                // The session owns the entire resource identity. A background
                // descendant must not outlive the registered leader.
                let _ = terminate_resource_identity(
                    &watched_dir,
                    &watched_manifest,
                    &watched_resource_backend,
                )
                .await;
                let _ = finalize_exited_manifest(
                    &watched_dir,
                    status.ok().and_then(|status| status.code()),
                );
                if let Ok(terminal) = load_manifest(&watched_dir) {
                    let _ = cleanup_terminal_resource_identity(
                        &watched_dir,
                        &terminal,
                        &watched_resource_backend,
                    );
                }
            });
            let governed_dir = session_dir.clone();
            let governed_resource_backend = self.resource_backend.clone();
            tokio::spawn(async move {
                supervise_process_governance(governed_dir, governed_resource_backend).await;
            });
            self.output_from_manifest(&manifest, 0, 0)
        }
    }

    pub async fn interact(
        &self,
        access: &ProcessSessionAccess,
        interaction: ProcessSessionInteraction,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        let session_dir = self.session_dir(interaction.session_id);
        let _ =
            sweep_process_session_retrying_conflicts(&session_dir, &self.resource_backend).await?;
        let mut manifest = load_manifest(&session_dir)?;
        self.validate_access(access, &manifest)?;
        if let ProcessSessionAction::Attach { max_bytes } = &interaction.action {
            let max_bytes = *max_bytes;
            if max_bytes == 0 || max_bytes > self.max_output_chunk_bytes {
                return Err(ProcessSessionError::InvalidRequest(format!(
                    "attach max_bytes must be between 1 and {}",
                    self.max_output_chunk_bytes
                )));
            }
            return self.output_tail_from_manifest(&manifest, max_bytes);
        }
        if manifest.state.is_terminal() {
            return self.output_from_manifest(
                &manifest,
                interaction.stdout_cursor,
                interaction.stderr_cursor,
            );
        }
        match interaction.action {
            ProcessSessionAction::Poll => {}
            ProcessSessionAction::Attach { .. } => unreachable!("handled above"),
            ProcessSessionAction::Write { bytes } => {
                if bytes.is_empty() || bytes.len() > MAX_STDIN_BYTES {
                    return Err(ProcessSessionError::InvalidRequest(
                        "stdin write must contain 1 to 65536 bytes".into(),
                    ));
                }
                ensure_attached_identity(&session_dir, &manifest, &self.resource_backend)?;
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.operation_sequence = current.operation_sequence.saturating_add(1);
                    current.last_operation = "stdin_write_intent".into();
                    current.last_input_digest = Some(sha256(&bytes));
                    let now = Utc::now();
                    current.last_activity_at = now;
                    current.updated_at = now;
                    Ok(())
                })?;
                match load_terminal_marker(&session_dir)? {
                    Some(marker) if marker.schema_version == 2 => {
                        let supervisor_id = marker
                            .supervisor_id
                            .ok_or(ProcessSessionError::Indeterminate)?;
                        let supervisor = self
                            .pty_supervisor
                            .as_ref()
                            .ok_or(ProcessSessionError::Indeterminate)?;
                        pty_supervisor::ensure_running(&self.state_root, supervisor).await?;
                        pty_supervisor::write(
                            &self.state_root,
                            interaction.session_id,
                            supervisor_id,
                            bytes,
                        )
                        .await?;
                    }
                    Some(_) => return Err(ProcessSessionError::Indeterminate),
                    None => write_fifo(&session_dir.join("stdin.fifo"), &bytes)?,
                }
                self.wake_wait_observer(interaction.session_id);
                manifest = load_manifest(&session_dir)?;
            }
            ProcessSessionAction::Resize { cols, rows } => {
                if cols == 0 || rows == 0 || cols > 2_000 || rows > 2_000 {
                    return Err(ProcessSessionError::InvalidRequest(
                        "terminal dimensions must be between 1 and 2000 cells".into(),
                    ));
                }
                ensure_attached_identity(&session_dir, &manifest, &self.resource_backend)?;
                let marker = load_terminal_marker(&session_dir)?
                    .filter(|marker| marker.schema_version == 2)
                    .ok_or_else(|| {
                        ProcessSessionError::InvalidRequest(
                            "process session is not controlled by a resumable PTY".into(),
                        )
                    })?;
                let supervisor_id = marker
                    .supervisor_id
                    .ok_or(ProcessSessionError::Indeterminate)?;
                let supervisor = self
                    .pty_supervisor
                    .as_ref()
                    .ok_or(ProcessSessionError::Indeterminate)?;
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.operation_sequence = current.operation_sequence.saturating_add(1);
                    current.last_operation = "terminal_resize_intent".into();
                    current.last_input_digest = Some(sha256(format!("{cols}x{rows}").as_bytes()));
                    let now = Utc::now();
                    current.last_activity_at = now;
                    current.updated_at = now;
                    Ok(())
                })?;
                pty_supervisor::ensure_running(&self.state_root, supervisor).await?;
                pty_supervisor::resize(
                    &self.state_root,
                    interaction.session_id,
                    supervisor_id,
                    cols,
                    rows,
                )
                .await?;
                manifest = load_manifest(&session_dir)?;
            }
            ProcessSessionAction::Interrupt => {
                let pgid =
                    ensure_attached_identity(&session_dir, &manifest, &self.resource_backend)?;
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.operation_sequence = current.operation_sequence.saturating_add(1);
                    current.last_operation = "interrupt_intent".into();
                    current.last_input_digest = None;
                    current.updated_at = Utc::now();
                    Ok(())
                })?;
                signal_group(pgid, libc::SIGINT)?;
                manifest = load_manifest(&session_dir)?;
            }
            ProcessSessionAction::Close => {
                ensure_attached_identity(&session_dir, &manifest, &self.resource_backend)?;
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.operation_sequence = current.operation_sequence.saturating_add(1);
                    current.last_operation = "close_intent".into();
                    current.last_input_digest = None;
                    current.state = ProcessSessionState::Terminating;
                    current.termination_reason = Some(ProcessSessionTerminationReason::Closed);
                    current.updated_at = Utc::now();
                    Ok(())
                })?;
                terminate_resource_identity(&session_dir, &manifest, &self.resource_backend)
                    .await?;
                mutate_manifest(&session_dir, |current| {
                    if !current.state.is_terminal() {
                        current.state = ProcessSessionState::Terminated;
                        current.resource_phase =
                            terminal_resource_phase(&current.resource_identity);
                        current.pid = None;
                        current.process_group_id = None;
                        current.exit_code = None;
                        current.last_operation = "closed".into();
                        current.termination_reason = Some(ProcessSessionTerminationReason::Closed);
                        current.updated_at = Utc::now();
                    }
                    Ok(())
                })?;
                manifest = load_manifest(&session_dir)?;
                cleanup_terminal_resource_identity(
                    &session_dir,
                    &manifest,
                    &self.resource_backend,
                )?;
            }
        }
        self.output_from_manifest(
            &manifest,
            interaction.stdout_cursor,
            interaction.stderr_cursor,
        )
    }

    pub async fn recover(
        &self,
        access: &ProcessSessionAccess,
        session_id: Uuid,
    ) -> Result<ProcessSessionRecovery, ProcessSessionError> {
        let session_dir = self.session_dir(session_id);
        let _ =
            sweep_process_session_retrying_conflicts(&session_dir, &self.resource_backend).await?;
        let mut manifest = load_manifest(&session_dir)?;
        self.validate_access(access, &manifest)?;
        if manifest.state.is_terminal() {
            return Ok(match manifest.state {
                ProcessSessionState::Indeterminate => ProcessSessionRecovery::Indeterminate,
                _ => ProcessSessionRecovery::Terminated,
            });
        }
        let terminal_control_available = match load_terminal_marker(&session_dir)? {
            Some(marker) if marker.schema_version == 2 => {
                let Some(supervisor_id) = marker.supervisor_id else {
                    return Err(ProcessSessionError::Indeterminate);
                };
                let Some(supervisor) = &self.pty_supervisor else {
                    return Err(ProcessSessionError::Indeterminate);
                };
                pty_supervisor::ensure_running(&self.state_root, supervisor).await?;
                pty_supervisor::status(&self.state_root, session_id, supervisor_id)
                    .await
                    .is_ok_and(|pid| pid == manifest.pid)
            }
            Some(_) => false,
            None => true,
        };
        if !terminal_control_available {
            terminate_resource_identity(&session_dir, &manifest, &self.resource_backend).await?;
            mutate_manifest(&session_dir, |current| {
                if current.session_id != manifest.session_id
                    || current.tenant_id != manifest.tenant_id
                    || current.workspace_root != manifest.workspace_root
                    || current.implementation_digest != manifest.implementation_digest
                    || current.governance_digest != manifest.governance_digest
                {
                    return Err(ProcessSessionError::Conflict);
                }
                current.state = ProcessSessionState::Indeterminate;
                current.resource_phase = terminal_resource_phase(&current.resource_identity);
                current.pid = None;
                current.process_group_id = None;
                current.exit_code = None;
                current.termination_reason = None;
                current.operation_sequence = current.operation_sequence.saturating_add(1);
                current.last_operation = "pty_control_lost".into();
                current.updated_at = Utc::now();
                Ok(())
            })?;
            return Ok(ProcessSessionRecovery::Indeterminate);
        }
        let identity_held = identity_is_held(&session_dir)?;
        let resource_alive = resource_identity_alive(&manifest, &self.resource_backend)?;
        let recovery = match (identity_held, resource_alive) {
            (true, true) => {
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.recovery_count = current.recovery_count.saturating_add(1);
                    current.last_operation = "reattached".into();
                    current.updated_at = Utc::now();
                    Ok(())
                })?;
                ProcessSessionRecovery::Reattached
            }
            (false, false) => {
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.state = ProcessSessionState::Terminated;
                    current.resource_phase = terminal_resource_phase(&current.resource_identity);
                    current.pid = None;
                    current.process_group_id = None;
                    current.exit_code = None;
                    current.last_operation = "terminated_before_recovery".into();
                    current.termination_reason =
                        Some(ProcessSessionTerminationReason::RecoveredMissing);
                    current.updated_at = Utc::now();
                    Ok(())
                })?;
                ProcessSessionRecovery::Terminated
            }
            _ => {
                mutate_manifest(&session_dir, |current| {
                    validate_same_manifest(current, &manifest)?;
                    current.state = ProcessSessionState::Indeterminate;
                    current.resource_phase = terminal_resource_phase(&current.resource_identity);
                    current.pid = None;
                    current.process_group_id = None;
                    current.exit_code = None;
                    current.last_operation = "identity_ambiguous".into();
                    current.updated_at = Utc::now();
                    Ok(())
                })?;
                ProcessSessionRecovery::Indeterminate
            }
        };
        manifest = load_manifest(&session_dir)?;
        if !manifest.is_well_formed() {
            return Err(ProcessSessionError::Indeterminate);
        }
        Ok(recovery)
    }

    pub async fn sweep(&self) -> Result<ProcessSessionSweepReport, ProcessSessionError> {
        let mut report = ProcessSessionSweepReport::default();
        for entry in std::fs::read_dir(self.sessions_root())
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
        {
            report.examined = report.examined.saturating_add(1);
            match sweep_process_session_retrying_conflicts(&entry.path(), &self.resource_backend)
                .await
            {
                Ok(ProcessSessionSweepOutcome::Active) => {
                    report.active = report.active.saturating_add(1);
                }
                Ok(ProcessSessionSweepOutcome::Terminated) => {
                    report.terminated = report.terminated.saturating_add(1);
                }
                Ok(ProcessSessionSweepOutcome::Indeterminate) | Err(_) => {
                    report.indeterminate = report.indeterminate.saturating_add(1);
                }
                Ok(ProcessSessionSweepOutcome::Terminal) => {}
            }
        }
        Ok(report)
    }

    fn validate_start(
        &self,
        request: &ProcessSessionStartRequest,
    ) -> Result<(), ProcessSessionError> {
        if request.session_id.is_nil()
            || request.context.tenant_id.is_nil()
            || request.context.run_id.is_nil()
            || request.context.attempt_id.is_nil()
            || request.initial_stdin.len() > MAX_STDIN_BYTES
            || request.context.cancellation.is_cancelled()
        {
            return Err(ProcessSessionError::InvalidRequest(
                "session identity, context or initial stdin is invalid".into(),
            ));
        }
        Ok(())
    }

    fn validate_access(
        &self,
        access: &ProcessSessionAccess,
        manifest: &ProcessSessionManifest,
    ) -> Result<(), ProcessSessionError> {
        let workspace = std::fs::canonicalize(&access.workspace_root)
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
        if access.tenant_id != manifest.tenant_id
            || workspace != manifest.workspace_root
            || manifest.implementation_digest != self.executor.implementation_digest()
            || manifest.resource_identity
                != ProcessSessionResourceIdentity::for_backend(
                    &self.resource_backend,
                    manifest.session_id,
                )
        {
            return Err(ProcessSessionError::AccessDenied);
        }
        Ok(())
    }

    fn initialize_session_files(&self, session_dir: &Path) -> Result<(), ProcessSessionError> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        for name in [
            "stdout.log",
            "stderr.log",
            "identity.lock",
            "control.lock",
            "sweep.lock",
        ] {
            let path = session_dir.join(name);
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
            options.open(path).map_err(io_error)?;
        }
        create_fifo(&session_dir.join("stdin.fifo"))
    }

    fn enforce_admission_quotas(
        &self,
        tenant_id: Uuid,
        workspace_root: &Path,
    ) -> Result<(), ProcessSessionError> {
        let mut global = 0usize;
        let mut tenant = 0usize;
        let mut workspace = 0usize;
        for entry in std::fs::read_dir(self.sessions_root())
            .map_err(|error| ProcessSessionError::Io(error.to_string()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
        {
            match load_manifest(&entry.path()) {
                Ok(manifest) if !manifest.state.is_terminal() => {
                    global = global.saturating_add(1);
                    if manifest.tenant_id == tenant_id {
                        tenant = tenant.saturating_add(1);
                        if manifest.workspace_root == workspace_root {
                            workspace = workspace.saturating_add(1);
                        }
                    }
                }
                // Malformed or incomplete state consumes global capacity until
                // a sweeper can reconcile it. It cannot be attributed safely
                // to a tenant or Workspace.
                Err(_) => global = global.saturating_add(1),
                Ok(_) => {}
            }
        }
        if global >= self.governance.max_active_sessions {
            return Err(ProcessSessionError::QuotaExceeded(
                ProcessSessionQuotaScope::Global,
            ));
        }
        if tenant >= self.governance.max_active_sessions_per_tenant {
            return Err(ProcessSessionError::QuotaExceeded(
                ProcessSessionQuotaScope::Tenant,
            ));
        }
        if workspace >= self.governance.max_active_sessions_per_workspace {
            return Err(ProcessSessionError::QuotaExceeded(
                ProcessSessionQuotaScope::Workspace,
            ));
        }
        Ok(())
    }

    fn output_from_manifest(
        &self,
        manifest: &ProcessSessionManifest,
        stdout_cursor: u64,
        stderr_cursor: u64,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        let session_dir = self.session_dir(manifest.session_id);
        let stdout_start_cursor = stdout_cursor;
        let stderr_start_cursor = stderr_cursor;
        let (stdout, stdout_cursor) = read_chunk(
            &session_dir.join("stdout.log"),
            stdout_cursor,
            self.max_output_chunk_bytes,
        )?;
        let (stderr, stderr_cursor) = read_chunk(
            &session_dir.join("stderr.log"),
            stderr_cursor,
            self.max_output_chunk_bytes,
        )?;
        Ok(ProcessSessionOutput {
            session_id: manifest.session_id,
            state: manifest.state,
            pid: (!manifest.state.is_terminal())
                .then_some(manifest.pid)
                .flatten(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_start_cursor,
            stderr_start_cursor,
            stdout_cursor,
            stderr_cursor,
            stdout_truncated: false,
            stderr_truncated: false,
            exit_code: manifest.exit_code,
            termination_reason: manifest.termination_reason,
        })
    }

    fn output_tail_from_manifest(
        &self,
        manifest: &ProcessSessionManifest,
        max_bytes: usize,
    ) -> Result<ProcessSessionOutput, ProcessSessionError> {
        let session_dir = self.session_dir(manifest.session_id);
        let (stdout, stdout_start_cursor, stdout_cursor, stdout_truncated) =
            read_tail(&session_dir.join("stdout.log"), max_bytes)?;
        let (stderr, stderr_start_cursor, stderr_cursor, stderr_truncated) =
            read_tail(&session_dir.join("stderr.log"), max_bytes)?;
        Ok(ProcessSessionOutput {
            session_id: manifest.session_id,
            state: manifest.state,
            pid: (!manifest.state.is_terminal())
                .then_some(manifest.pid)
                .flatten(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_start_cursor,
            stderr_start_cursor,
            stdout_cursor,
            stderr_cursor,
            stdout_truncated,
            stderr_truncated,
            exit_code: manifest.exit_code,
            termination_reason: manifest.termination_reason,
        })
    }

    fn sessions_root(&self) -> PathBuf {
        self.state_root.join("process-sessions")
    }

    fn session_dir(&self, session_id: Uuid) -> PathBuf {
        self.sessions_root().join(session_id.to_string())
    }
}

#[cfg(unix)]
fn ensure_owner_only_directory(path: &Path) -> Result<(), ProcessSessionError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(ProcessSessionError::InvalidConfiguration(format!(
            "{} must be an owner-controlled directory",
            path.display()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_error)
}

#[cfg(not(unix))]
fn ensure_owner_only_directory(_path: &Path) -> Result<(), ProcessSessionError> {
    Err(ProcessSessionError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_private_staging_file(path: &Path) -> Result<File, ProcessSessionError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(io_error)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(io_error)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_staging_file(_path: &Path) -> Result<File, ProcessSessionError> {
    Err(ProcessSessionError::UnsupportedPlatform)
}

fn persisted(manifest: &ProcessSessionManifest) -> PersistedProcessSessionManifest {
    PersistedProcessSessionManifest {
        manifest: manifest.clone(),
        digest: manifest_digest(manifest),
    }
}

fn persist_manifest(
    session_dir: &Path,
    manifest: &ProcessSessionManifest,
) -> Result<(), ProcessSessionError> {
    if !manifest.is_well_formed() {
        return Err(ProcessSessionError::InvalidRequest(
            "refusing to persist malformed process session state".into(),
        ));
    }
    let path = session_dir.join("manifest.json");
    let staging = session_dir.join("manifest.json.partial");
    let bytes = serde_json::to_vec_pretty(&persisted(manifest))
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    let mut file = open_private_staging_file(&staging)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(staging, path).map_err(io_error)?;
    File::open(session_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error)
}

fn persist_terminal_marker(
    session_dir: &Path,
    marker: &ProcessTerminalMarker,
) -> Result<(), ProcessSessionError> {
    if !marker.is_well_formed() {
        return Err(ProcessSessionError::InvalidRequest(
            "refusing to persist an invalid PTY marker".into(),
        ));
    }
    let bytes = serde_json::to_vec(marker).map_err(|error| {
        ProcessSessionError::Io(format!("failed to encode PTY marker: {error}"))
    })?;
    let persisted = PersistedProcessTerminalMarker {
        marker: marker.clone(),
        digest: sha256(&bytes),
    };
    let path = session_dir.join("terminal.json");
    let staging = session_dir.join("terminal.json.partial");
    let bytes = serde_json::to_vec_pretty(&persisted)
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    let mut file = open_private_staging_file(&staging)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(staging, path).map_err(io_error)?;
    File::open(session_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error)
}

fn load_terminal_marker(
    session_dir: &Path,
) -> Result<Option<ProcessTerminalMarker>, ProcessSessionError> {
    let bytes = match std::fs::read(session_dir.join("terminal.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let persisted: PersistedProcessTerminalMarker =
        serde_json::from_slice(&bytes).map_err(|_| ProcessSessionError::Indeterminate)?;
    let marker_bytes =
        serde_json::to_vec(&persisted.marker).map_err(|_| ProcessSessionError::Indeterminate)?;
    if !persisted.marker.is_well_formed() || persisted.digest != sha256(&marker_bytes) {
        return Err(ProcessSessionError::Indeterminate);
    }
    Ok(Some(persisted.marker))
}

fn interaction_receipt_digest(receipt: &ProcessInteractionReceipt) -> String {
    let bytes = serde_json::to_vec(receipt).expect("process interaction receipt is serializable");
    sha256(&bytes)
}

fn persist_interaction_receipt(
    session_dir: &Path,
    receipt: &ProcessInteractionReceipt,
) -> Result<(), ProcessSessionError> {
    if !receipt.is_well_formed() {
        return Err(ProcessSessionError::InvalidRequest(
            "refusing to persist an invalid process interaction receipt".into(),
        ));
    }
    let receipts_dir = session_dir.join("interaction-receipts");
    std::fs::create_dir_all(&receipts_dir).map_err(io_error)?;
    ensure_owner_only_directory(&receipts_dir)?;
    let file_name = format!("{}.json", receipt.source_binding_digest);
    let path = receipts_dir.join(&file_name);
    if path.is_file() {
        let existing = load_interaction_receipt(session_dir, &receipt.source_binding_digest)?;
        return if existing == *receipt {
            Ok(())
        } else {
            Err(ProcessSessionError::Conflict)
        };
    }
    let persisted = PersistedProcessInteractionReceipt {
        receipt: receipt.clone(),
        digest: interaction_receipt_digest(receipt),
    };
    let staging = receipts_dir.join(format!("{file_name}.partial"));
    let bytes = serde_json::to_vec_pretty(&persisted)
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    let mut file = open_private_staging_file(&staging)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    std::fs::rename(staging, path).map_err(io_error)?;
    File::open(&receipts_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error)
}

fn load_interaction_receipt(
    session_dir: &Path,
    binding_digest: &str,
) -> Result<ProcessInteractionReceipt, ProcessSessionError> {
    if !is_sha256(binding_digest) {
        return Err(ProcessSessionError::InvalidRequest(
            "process interaction binding digest is invalid".into(),
        ));
    }
    let bytes = std::fs::read(
        session_dir
            .join("interaction-receipts")
            .join(format!("{binding_digest}.json")),
    )
    .map_err(io_error)?;
    let persisted: PersistedProcessInteractionReceipt =
        serde_json::from_slice(&bytes).map_err(|_| ProcessSessionError::Indeterminate)?;
    if !persisted.receipt.is_well_formed()
        || persisted.receipt.source_binding_digest != binding_digest
        || persisted.digest != interaction_receipt_digest(&persisted.receipt)
    {
        return Err(ProcessSessionError::Indeterminate);
    }
    Ok(persisted.receipt)
}

fn load_manifest(session_dir: &Path) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let bytes = match std::fs::read(session_dir.join("manifest.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProcessSessionError::NotFound);
        }
        Err(error) => return Err(io_error(error)),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
    match value["manifest"]["schema_version"].as_u64() {
        Some(version) if version == u64::from(PROCESS_SESSION_SCHEMA_VERSION) => {
            let persisted: PersistedProcessSessionManifest = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            if !persisted.manifest.is_well_formed()
                || manifest_digest(&persisted.manifest) != persisted.digest
            {
                return Err(ProcessSessionError::Indeterminate);
            }
            Ok(persisted.manifest)
        }
        Some(6) => {
            let persisted: PersistedProcessSessionManifest = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            if manifest_digest(&persisted.manifest) != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_schema_six(persisted.manifest)
        }
        Some(5) => {
            let persisted: PersistedProcessSessionManifest = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            if manifest_digest(&persisted.manifest) != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_schema_five(persisted.manifest)
        }
        Some(4) => {
            let persisted: PersistedProcessSessionManifest = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            if manifest_digest(&persisted.manifest) != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_schema_four(persisted.manifest)
        }
        Some(3) => {
            let persisted: PersistedLegacyProcessSessionManifestV3 = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            let digest = sha256(
                &serde_json::to_vec(&persisted.manifest)
                    .map_err(|error| ProcessSessionError::Io(error.to_string()))?,
            );
            if digest != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_schema_three(persisted.manifest)
        }
        Some(2) => {
            let persisted: PersistedLegacyProcessSessionManifestV2 = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            let digest = sha256(
                &serde_json::to_vec(&persisted.manifest)
                    .map_err(|error| ProcessSessionError::Io(error.to_string()))?,
            );
            if digest != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_schema_two(persisted.manifest)
        }
        Some(1) => {
            let persisted: PersistedLegacyProcessSessionManifest = serde_json::from_value(value)
                .map_err(|error| ProcessSessionError::Io(error.to_string()))?;
            let digest = sha256(
                &serde_json::to_vec(&persisted.manifest)
                    .map_err(|error| ProcessSessionError::Io(error.to_string()))?,
            );
            if !persisted.manifest.is_well_formed_terminal() || digest != persisted.digest {
                return Err(ProcessSessionError::Indeterminate);
            }
            migrate_legacy_terminal(persisted.manifest)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn migrate_schema_six(
    mut legacy: ProcessSessionManifest,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    legacy.schema_version = PROCESS_SESSION_SCHEMA_VERSION;
    if legacy.is_well_formed() {
        Ok(legacy)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn migrate_schema_five(
    mut legacy: ProcessSessionManifest,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    legacy.schema_version = PROCESS_SESSION_SCHEMA_VERSION;
    if legacy.is_well_formed() {
        Ok(legacy)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn migrate_schema_four(
    mut legacy: ProcessSessionManifest,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    legacy.schema_version = PROCESS_SESSION_SCHEMA_VERSION;
    if legacy.state == ProcessSessionState::Starting {
        legacy.resource_phase = ProcessSessionResourcePhase::LegacyUnknown;
    }
    if legacy.is_well_formed() {
        Ok(legacy)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn migrate_schema_three(
    legacy: LegacyProcessSessionManifestV3,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let resource_phase = migrated_resource_phase(legacy.state, &legacy.resource_identity);
    let migrated = ProcessSessionManifest {
        schema_version: PROCESS_SESSION_SCHEMA_VERSION,
        session_id: legacy.session_id,
        tenant_id: legacy.tenant_id,
        workspace_root: legacy.workspace_root,
        source_run_id: legacy.source_run_id,
        source_attempt_id: legacy.source_attempt_id,
        source_tool_call_id: legacy.source_tool_call_id,
        source_binding_digest: legacy.source_binding_digest,
        implementation_digest: legacy.implementation_digest,
        governance_digest: legacy.governance_digest,
        resource_identity: legacy.resource_identity,
        resource_phase,
        state: legacy.state,
        pid: legacy.pid,
        process_group_id: legacy.process_group_id,
        exit_code: legacy.exit_code,
        operation_sequence: legacy.operation_sequence,
        last_operation: legacy.last_operation,
        last_input_digest: legacy.last_input_digest,
        recovery_count: legacy.recovery_count,
        started_at: legacy.started_at,
        execution_deadline_at: legacy.execution_deadline_at,
        idle_timeout_millis: legacy.idle_timeout_millis,
        last_activity_at: legacy.last_activity_at,
        max_output_bytes_per_stream: legacy.max_output_bytes_per_stream,
        max_cpu_seconds: legacy.max_cpu_seconds,
        max_memory_bytes: legacy.max_memory_bytes,
        observed_cpu_usage_micros: legacy.observed_cpu_usage_micros,
        observed_stdout_bytes: legacy.observed_stdout_bytes,
        observed_stderr_bytes: legacy.observed_stderr_bytes,
        termination_reason: legacy.termination_reason,
        updated_at: legacy.updated_at,
    };
    if migrated.is_well_formed() {
        Ok(migrated)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn migrated_resource_phase(
    state: ProcessSessionState,
    resource_identity: &ProcessSessionResourceIdentity,
) -> ProcessSessionResourcePhase {
    match (state, resource_identity) {
        (ProcessSessionState::Starting, _) => ProcessSessionResourcePhase::LegacyUnknown,
        (ProcessSessionState::Running | ProcessSessionState::Terminating, _) => {
            ProcessSessionResourcePhase::Active
        }
        (_, ProcessSessionResourceIdentity::LinuxCgroupV2 { .. }) => {
            ProcessSessionResourcePhase::CleanupPending
        }
        (_, ProcessSessionResourceIdentity::UnixRlimit) => ProcessSessionResourcePhase::Cleaned,
    }
}

fn migrate_schema_two(
    legacy: LegacyProcessSessionManifestV2,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let migrated = ProcessSessionManifest {
        schema_version: PROCESS_SESSION_SCHEMA_VERSION,
        session_id: legacy.session_id,
        tenant_id: legacy.tenant_id,
        workspace_root: legacy.workspace_root,
        source_run_id: legacy.source_run_id,
        source_attempt_id: legacy.source_attempt_id,
        source_tool_call_id: legacy.source_tool_call_id,
        source_binding_digest: legacy.source_binding_digest,
        implementation_digest: legacy.implementation_digest,
        governance_digest: legacy.governance_digest,
        resource_identity: ProcessSessionResourceIdentity::UnixRlimit,
        resource_phase: migrated_resource_phase(
            legacy.state,
            &ProcessSessionResourceIdentity::UnixRlimit,
        ),
        state: legacy.state,
        pid: legacy.pid,
        process_group_id: legacy.process_group_id,
        exit_code: legacy.exit_code,
        operation_sequence: legacy.operation_sequence,
        last_operation: legacy.last_operation,
        last_input_digest: legacy.last_input_digest,
        recovery_count: legacy.recovery_count,
        started_at: legacy.started_at,
        execution_deadline_at: legacy.execution_deadline_at,
        idle_timeout_millis: legacy.idle_timeout_millis,
        last_activity_at: legacy.last_activity_at,
        max_output_bytes_per_stream: legacy.max_output_bytes_per_stream,
        max_cpu_seconds: legacy.max_cpu_seconds,
        max_memory_bytes: legacy.max_memory_bytes,
        observed_cpu_usage_micros: 0,
        observed_stdout_bytes: legacy.observed_stdout_bytes,
        observed_stderr_bytes: legacy.observed_stderr_bytes,
        termination_reason: legacy.termination_reason,
        updated_at: legacy.updated_at,
    };
    if migrated.is_well_formed() {
        Ok(migrated)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn migrate_legacy_terminal(
    legacy: LegacyProcessSessionManifest,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let governance = ProcessSessionGovernance::default();
    let execution_deadline_at = legacy
        .started_at
        .checked_add_signed(
            chrono::Duration::from_std(governance.max_runtime)
                .map_err(|_| ProcessSessionError::Indeterminate)?,
        )
        .ok_or(ProcessSessionError::Indeterminate)?;
    let termination_reason = (legacy.state == ProcessSessionState::Terminated)
        .then_some(ProcessSessionTerminationReason::LegacyTerminal);
    let migrated = ProcessSessionManifest {
        schema_version: PROCESS_SESSION_SCHEMA_VERSION,
        session_id: legacy.session_id,
        tenant_id: legacy.tenant_id,
        workspace_root: legacy.workspace_root,
        source_run_id: legacy.source_run_id,
        source_attempt_id: legacy.source_attempt_id,
        source_tool_call_id: legacy.source_tool_call_id,
        source_binding_digest: legacy.source_binding_digest,
        implementation_digest: legacy.implementation_digest,
        governance_digest: governance_digest(
            &governance,
            ProcessSessionResourceCapabilities::current(),
        ),
        resource_identity: ProcessSessionResourceIdentity::UnixRlimit,
        resource_phase: ProcessSessionResourcePhase::Cleaned,
        state: legacy.state,
        pid: None,
        process_group_id: None,
        exit_code: legacy.exit_code,
        operation_sequence: legacy.operation_sequence,
        last_operation: legacy.last_operation,
        last_input_digest: legacy.last_input_digest,
        recovery_count: legacy.recovery_count,
        started_at: legacy.started_at,
        execution_deadline_at,
        idle_timeout_millis: u64::try_from(governance.idle_timeout.as_millis())
            .map_err(|_| ProcessSessionError::Indeterminate)?,
        last_activity_at: legacy.updated_at,
        max_output_bytes_per_stream: governance.max_output_bytes_per_stream,
        max_cpu_seconds: governance.max_cpu_seconds,
        max_memory_bytes: governance.max_memory_bytes,
        observed_cpu_usage_micros: 0,
        observed_stdout_bytes: 0,
        observed_stderr_bytes: 0,
        termination_reason,
        updated_at: legacy.updated_at,
    };
    if migrated.is_well_formed() {
        Ok(migrated)
    } else {
        Err(ProcessSessionError::Indeterminate)
    }
}

fn mutate_manifest(
    session_dir: &Path,
    update: impl FnOnce(&mut ProcessSessionManifest) -> Result<(), ProcessSessionError>,
) -> Result<(), ProcessSessionError> {
    let control = OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_dir.join("control.lock"))
        .map_err(io_error)?;
    lock_exclusive(&control)?;
    let mut manifest = load_manifest(session_dir)?;
    let result = update(&mut manifest).and_then(|()| persist_manifest(session_dir, &manifest));
    unlock(&control)?;
    result
}

fn validate_same_manifest(
    current: &ProcessSessionManifest,
    expected: &ProcessSessionManifest,
) -> Result<(), ProcessSessionError> {
    if current.session_id != expected.session_id
        || current.tenant_id != expected.tenant_id
        || current.workspace_root != expected.workspace_root
        || current.implementation_digest != expected.implementation_digest
        || current.governance_digest != expected.governance_digest
        || current.operation_sequence != expected.operation_sequence
        || current.resource_phase != expected.resource_phase
        || current.state != expected.state
    {
        return Err(ProcessSessionError::Conflict);
    }
    Ok(())
}

fn finalize_exited_manifest(
    session_dir: &Path,
    exit_code: Option<i32>,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |manifest| {
        if !manifest.state.is_terminal() {
            let output_limited =
                session_output_lengths(session_dir).is_ok_and(|(stdout_bytes, stderr_bytes)| {
                    stdout_bytes >= manifest.max_output_bytes_per_stream
                        || stderr_bytes >= manifest.max_output_bytes_per_stream
                });
            if output_limited {
                manifest.state = ProcessSessionState::Terminated;
                manifest.termination_reason = Some(ProcessSessionTerminationReason::OutputLimit);
            } else if manifest.state == ProcessSessionState::Terminating {
                manifest.state = ProcessSessionState::Terminated;
                if manifest.termination_reason.is_none() {
                    manifest.termination_reason =
                        Some(ProcessSessionTerminationReason::RecoveredMissing);
                }
            } else {
                manifest.state = ProcessSessionState::Exited;
                manifest.termination_reason = None;
            }
            manifest.resource_phase = terminal_resource_phase(&manifest.resource_identity);
            manifest.pid = None;
            manifest.process_group_id = None;
            manifest.exit_code = exit_code;
            manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
            manifest.last_operation = "exited".into();
            manifest.last_input_digest = None;
            manifest.updated_at = Utc::now();
        }
        Ok(())
    })
}

fn validate_governance(
    governance: &ProcessSessionGovernance,
    max_output_chunk_bytes: usize,
    capabilities: ProcessSessionResourceCapabilities,
) -> Result<(), ProcessSessionError> {
    let max_runtime = Duration::from_secs(24 * 60 * 60);
    let max_output_bytes = 1024_u64 * 1024 * 1024;
    if governance.max_active_sessions == 0
        || governance.max_active_sessions > MAX_PROCESS_SESSIONS
        || governance.max_active_sessions_per_tenant == 0
        || governance.max_active_sessions_per_tenant > governance.max_active_sessions
        || governance.max_active_sessions_per_workspace == 0
        || governance.max_active_sessions_per_workspace > governance.max_active_sessions_per_tenant
        || governance.max_runtime.is_zero()
        || governance.max_runtime > max_runtime
        || governance.idle_timeout.is_zero()
        || governance.idle_timeout > max_runtime
        || governance.max_output_bytes_per_stream
            < u64::try_from(max_output_chunk_bytes).unwrap_or(u64::MAX)
        || governance.max_output_bytes_per_stream > max_output_bytes
        || governance.max_cpu_seconds == 0
        || governance.max_cpu_seconds > 24 * 60 * 60
        || governance
            .max_memory_bytes
            .is_some_and(|bytes| !(64 * 1024 * 1024..=64_u64 * 1024 * 1024 * 1024).contains(&bytes))
        || governance
            .max_processes
            .is_some_and(|processes| !(1..=4096).contains(&processes))
    {
        return Err(ProcessSessionError::InvalidConfiguration(
            "process governance limits are invalid".into(),
        ));
    }
    if !capabilities.hard_output_file_limit {
        return Err(ProcessSessionError::UnsupportedResourceCapability(
            "hard_output_file_limit",
        ));
    }
    if !capabilities.hard_cpu_time_limit {
        return Err(ProcessSessionError::UnsupportedResourceCapability(
            "hard_cpu_time_limit",
        ));
    }
    if governance.max_memory_bytes.is_some() && !capabilities.hard_memory_limit {
        return Err(ProcessSessionError::UnsupportedResourceCapability(
            "hard_memory_limit",
        ));
    }
    if governance.max_processes.is_some() && !capabilities.hard_process_count_limit {
        return Err(ProcessSessionError::UnsupportedResourceCapability(
            "hard_process_count_limit",
        ));
    }
    if governance.require_whole_process_tree_accounting
        && !capabilities.whole_process_tree_accounting
    {
        return Err(ProcessSessionError::UnsupportedResourceCapability(
            "whole_process_tree_accounting",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessSessionSweepOutcome {
    Active,
    Terminal,
    Terminated,
    Indeterminate,
}

async fn supervise_process_governance(
    session_dir: PathBuf,
    resource_backend: ProcessSessionResourceBackend,
) {
    loop {
        match sweep_process_session_retrying_conflicts(&session_dir, &resource_backend).await {
            Ok(ProcessSessionSweepOutcome::Active) => {
                let Ok(manifest) = load_manifest(&session_dir) else {
                    return;
                };
                tokio::time::sleep(next_governance_check_delay(&manifest, Utc::now())).await;
            }
            Ok(_) | Err(_) => return,
        }
    }
}

async fn sweep_process_session_retrying_conflicts(
    session_dir: &Path,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<ProcessSessionSweepOutcome, ProcessSessionError> {
    for attempt in 0..5 {
        match sweep_process_session(session_dir, resource_backend).await {
            Err(ProcessSessionError::Conflict) if attempt < 4 => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded process-session sweep retry loop always returns")
}

fn next_governance_check_delay(manifest: &ProcessSessionManifest, now: DateTime<Utc>) -> Duration {
    let execution_remaining = (manifest.execution_deadline_at - now)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let idle_remaining = i64::try_from(manifest.idle_timeout_millis)
        .ok()
        .and_then(|millis| {
            manifest
                .last_activity_at
                .checked_add_signed(chrono::Duration::milliseconds(millis))
        })
        .map(|deadline| (deadline - now).to_std().unwrap_or(Duration::ZERO))
        .unwrap_or(Duration::ZERO);
    execution_remaining
        .min(idle_remaining)
        .min(MAX_GOVERNANCE_CHECK_INTERVAL)
        .max(MIN_GOVERNANCE_CHECK_INTERVAL)
}

async fn sweep_process_session(
    session_dir: &Path,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<ProcessSessionSweepOutcome, ProcessSessionError> {
    let initial = load_manifest(session_dir)?;
    if initial.state.is_terminal() {
        cleanup_terminal_resource_identity(session_dir, &initial, resource_backend)?;
        return Ok(ProcessSessionSweepOutcome::Terminal);
    }
    let _sweep_guard = acquire_session_sweep_lock(session_dir).await?;
    let initial = load_manifest(session_dir)?;
    if initial.state.is_terminal() {
        cleanup_terminal_resource_identity(session_dir, &initial, resource_backend)?;
        return Ok(ProcessSessionSweepOutcome::Terminal);
    }
    let mut manifest = if initial.state == ProcessSessionState::Starting {
        refresh_process_activity(session_dir)?
    } else {
        refresh_process_activity_with_resources(session_dir, resource_backend)?
    };
    let mut identity_held = identity_is_held(session_dir)?;
    let mut resource_alive = match resource_identity_alive(&manifest, resource_backend) {
        Ok(alive) => alive,
        Err(error) if manifest.state == ProcessSessionState::Starting => {
            return quarantine_unpublished_starting_resource(
                session_dir,
                &manifest,
                resource_backend,
                error,
            );
        }
        Err(error) => return Err(error),
    };
    if identity_held != resource_alive {
        let transition_deadline = tokio::time::Instant::now() + RESOURCE_IDENTITY_TRANSITION_GRACE;
        while tokio::time::Instant::now() < transition_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            manifest = load_manifest(session_dir)?;
            if manifest.state.is_terminal() {
                return Ok(ProcessSessionSweepOutcome::Terminal);
            }
            identity_held = identity_is_held(session_dir)?;
            resource_alive = match resource_identity_alive(&manifest, resource_backend) {
                Ok(alive) => alive,
                Err(error) if manifest.state == ProcessSessionState::Starting => {
                    return quarantine_unpublished_starting_resource(
                        session_dir,
                        &manifest,
                        resource_backend,
                        error,
                    );
                }
                Err(error) => return Err(error),
            };
            if identity_held == resource_alive {
                break;
            }
        }
    }
    if manifest.state == ProcessSessionState::Starting
        && !resource_alive
        && manifest.resource_phase != ProcessSessionResourcePhase::Unprepared
    {
        // `prepared` means spawn may already have returned, while legacy
        // manifests never recorded which side of spawn they reached. An empty
        // or missing identity therefore cannot prove that a fast Tool did not
        // execute. A best-effort kill closes a still-addressable Linux group;
        // the durable result remains indeterminate either way.
        let _ = kill_unpublished_starting_resource(&manifest, resource_backend);
        mark_identity_indeterminate(session_dir, &manifest)?;
        return Ok(ProcessSessionSweepOutcome::Indeterminate);
    }
    if manifest.state == ProcessSessionState::Starting && resource_alive {
        kill_unpublished_starting_resource(&manifest, resource_backend)?;
        mark_identity_indeterminate(session_dir, &manifest)?;
        return Ok(ProcessSessionSweepOutcome::Indeterminate);
    }
    match (identity_held, resource_alive) {
        (false, false) => {
            finalize_missing_process(session_dir, &manifest)?;
            let terminal = load_manifest(session_dir)?;
            cleanup_terminal_resource_identity(session_dir, &terminal, resource_backend)?;
            return Ok(ProcessSessionSweepOutcome::Terminated);
        }
        (true, true) => {}
        _ => {
            mark_identity_indeterminate(session_dir, &manifest)?;
            return Ok(ProcessSessionSweepOutcome::Indeterminate);
        }
    }

    let reason = if manifest.state == ProcessSessionState::Terminating {
        manifest.termination_reason
    } else {
        governance_termination_reason(&manifest, Utc::now())?
    };
    let Some(reason) = reason else {
        return Ok(ProcessSessionSweepOutcome::Active);
    };
    if manifest.state == ProcessSessionState::Running {
        begin_governance_termination(session_dir, &manifest, reason)?;
    }
    terminate_resource_identity(session_dir, &manifest, resource_backend).await?;
    finalize_governance_termination(session_dir, reason)?;
    let terminal = load_manifest(session_dir)?;
    cleanup_terminal_resource_identity(session_dir, &terminal, resource_backend)?;
    Ok(ProcessSessionSweepOutcome::Terminated)
}

fn kill_unpublished_starting_resource(
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<(), ProcessSessionError> {
    if manifest.state != ProcessSessionState::Starting {
        return Err(ProcessSessionError::Indeterminate);
    }
    match (resource_backend, &manifest.resource_identity) {
        (ProcessSessionResourceBackend::LinuxCgroupV2 { .. }, _) => {
            let group = open_linux_cgroup(resource_backend, manifest)?;
            kill_linux_cgroup_v2_group(&group).map_err(|_| ProcessSessionError::Indeterminate)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn quarantine_unpublished_starting_resource(
    session_dir: &Path,
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
    probe_error: ProcessSessionError,
) -> Result<ProcessSessionSweepOutcome, ProcessSessionError> {
    let kill_result = kill_unpublished_starting_resource(manifest, resource_backend);
    mark_identity_indeterminate(session_dir, manifest)?;
    kill_result.map_err(|_| probe_error)?;
    Ok(ProcessSessionSweepOutcome::Indeterminate)
}

async fn acquire_session_sweep_lock(session_dir: &Path) -> Result<File, ProcessSessionError> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options
        .open(session_dir.join("sweep.lock"))
        .map_err(io_error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match lock_exclusive_nonblocking(&file) {
            Ok(()) => return Ok(file),
            Err(ProcessSessionError::Conflict) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(ProcessSessionError::Conflict) => {
                return Err(ProcessSessionError::Conflict);
            }
            Err(error) => return Err(error),
        }
    }
}

fn governance_termination_reason(
    manifest: &ProcessSessionManifest,
    now: DateTime<Utc>,
) -> Result<Option<ProcessSessionTerminationReason>, ProcessSessionError> {
    let idle_millis = i64::try_from(manifest.idle_timeout_millis)
        .map_err(|_| ProcessSessionError::Indeterminate)?;
    let idle_deadline = manifest
        .last_activity_at
        .checked_add_signed(chrono::Duration::milliseconds(idle_millis))
        .ok_or(ProcessSessionError::Indeterminate)?;
    if manifest.observed_stdout_bytes >= manifest.max_output_bytes_per_stream
        || manifest.observed_stderr_bytes >= manifest.max_output_bytes_per_stream
    {
        Ok(Some(ProcessSessionTerminationReason::OutputLimit))
    } else if manifest.observed_cpu_usage_micros
        >= manifest
            .max_cpu_seconds
            .checked_mul(1_000_000)
            .ok_or(ProcessSessionError::Indeterminate)?
    {
        Ok(Some(ProcessSessionTerminationReason::CpuLimit))
    } else if now >= manifest.execution_deadline_at
        && manifest.execution_deadline_at <= idle_deadline
    {
        Ok(Some(ProcessSessionTerminationReason::ExecutionDeadline))
    } else if now >= idle_deadline {
        Ok(Some(ProcessSessionTerminationReason::IdleTimeout))
    } else {
        Ok(None)
    }
}

fn begin_governance_termination(
    session_dir: &Path,
    expected: &ProcessSessionManifest,
    reason: ProcessSessionTerminationReason,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |current| {
        validate_same_manifest(current, expected)?;
        if current.state != ProcessSessionState::Running {
            return Err(ProcessSessionError::Conflict);
        }
        current.state = ProcessSessionState::Terminating;
        current.operation_sequence = current.operation_sequence.saturating_add(1);
        current.last_operation = match reason {
            ProcessSessionTerminationReason::ExecutionDeadline => "execution_deadline_intent",
            ProcessSessionTerminationReason::CpuLimit => "cpu_limit_intent",
            ProcessSessionTerminationReason::IdleTimeout => "idle_timeout_intent",
            ProcessSessionTerminationReason::OutputLimit => "output_limit_intent",
            ProcessSessionTerminationReason::StartFailed => "start_failed_intent",
            ProcessSessionTerminationReason::Closed => "close_intent",
            ProcessSessionTerminationReason::RecoveredMissing => "recovered_missing_intent",
            ProcessSessionTerminationReason::LegacyTerminal => "legacy_terminal_intent",
        }
        .into();
        current.termination_reason = Some(reason);
        current.updated_at = Utc::now().max(current.updated_at);
        Ok(())
    })
}

fn finalize_missing_process(
    session_dir: &Path,
    expected: &ProcessSessionManifest,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |current| {
        validate_same_manifest(current, expected)?;
        current.state = ProcessSessionState::Terminated;
        current.resource_phase = terminal_resource_phase(&current.resource_identity);
        current.pid = None;
        current.process_group_id = None;
        current.exit_code = None;
        current.operation_sequence = current.operation_sequence.saturating_add(1);
        current.last_operation = "sweeper_found_process_missing".into();
        current.last_input_digest = None;
        current.termination_reason = Some(ProcessSessionTerminationReason::RecoveredMissing);
        current.updated_at = Utc::now().max(current.updated_at);
        Ok(())
    })
}

fn refresh_process_activity(
    session_dir: &Path,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let (stdout_bytes, stderr_bytes) = session_output_lengths(session_dir)?;
    let mut activity_at = None;
    let current = load_manifest(session_dir)?;
    if stdout_bytes != current.observed_stdout_bytes {
        activity_at = Some(file_modified_at(&session_dir.join("stdout.log"))?);
    }
    if stderr_bytes != current.observed_stderr_bytes {
        let stderr_at = file_modified_at(&session_dir.join("stderr.log"))?;
        activity_at = Some(activity_at.map_or(stderr_at, |value| value.max(stderr_at)));
    }
    if stdout_bytes < current.observed_stdout_bytes || stderr_bytes < current.observed_stderr_bytes
    {
        mark_identity_indeterminate(session_dir, &current)?;
        return Err(ProcessSessionError::Indeterminate);
    }
    if stdout_bytes == current.observed_stdout_bytes
        && stderr_bytes == current.observed_stderr_bytes
    {
        return Ok(current);
    }
    mutate_manifest(session_dir, |manifest| {
        validate_same_manifest(manifest, &current)?;
        manifest.observed_stdout_bytes = stdout_bytes;
        manifest.observed_stderr_bytes = stderr_bytes;
        if let Some(activity_at) = activity_at {
            manifest.last_activity_at = manifest.last_activity_at.max(activity_at);
            manifest.updated_at = manifest.updated_at.max(activity_at);
        }
        Ok(())
    })?;
    load_manifest(session_dir)
}

fn refresh_process_activity_with_resources(
    session_dir: &Path,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<ProcessSessionManifest, ProcessSessionError> {
    let current = refresh_process_activity(session_dir)?;
    let cpu_usage_micros = match (resource_backend, &current.resource_identity) {
        (ProcessSessionResourceBackend::UnixRlimit, ProcessSessionResourceIdentity::UnixRlimit) => {
            return Ok(current);
        }
        (ProcessSessionResourceBackend::LinuxCgroupV2 { .. }, _) => {
            let group = open_linux_cgroup(resource_backend, &current)?;
            read_linux_cgroup_cpu_usage_micros_group(&group)
                .map_err(|_| ProcessSessionError::Indeterminate)?
        }
        _ => return Err(ProcessSessionError::Indeterminate),
    };
    if cpu_usage_micros < current.observed_cpu_usage_micros {
        mark_identity_indeterminate(session_dir, &current)?;
        return Err(ProcessSessionError::Indeterminate);
    }
    if cpu_usage_micros == current.observed_cpu_usage_micros {
        return Ok(current);
    }
    mutate_manifest(session_dir, |manifest| {
        validate_same_manifest(manifest, &current)?;
        manifest.observed_cpu_usage_micros = cpu_usage_micros;
        manifest.updated_at = Utc::now().max(manifest.updated_at);
        Ok(())
    })?;
    load_manifest(session_dir)
}

fn open_linux_cgroup(
    resource_backend: &ProcessSessionResourceBackend,
    manifest: &ProcessSessionManifest,
) -> Result<LinuxCgroupV2Group, ProcessSessionError> {
    match (resource_backend, &manifest.resource_identity) {
        (
            ProcessSessionResourceBackend::LinuxCgroupV2 { root },
            ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
        ) if manifest
            .resource_identity
            .is_well_formed_for(manifest.session_id) =>
        {
            root.open_group(group_name)
                .map_err(|_| ProcessSessionError::Indeterminate)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn resource_identity_alive(
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<bool, ProcessSessionError> {
    match (resource_backend, &manifest.resource_identity) {
        (ProcessSessionResourceBackend::UnixRlimit, ProcessSessionResourceIdentity::UnixRlimit) => {
            Ok(manifest.process_group_id.is_some_and(process_group_alive))
        }
        (
            ProcessSessionResourceBackend::LinuxCgroupV2 { root },
            ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
        ) if manifest
            .resource_identity
            .is_well_formed_for(manifest.session_id) =>
        {
            let group = match root.open_group(group_name) {
                Ok(group) => group,
                Err(ProcessResourceError::GroupMissing(_)) => return Ok(false),
                Err(_) => return Err(ProcessSessionError::Indeterminate),
            };
            read_linux_cgroup_populated_group(&group)
                .map_err(|_| ProcessSessionError::Indeterminate)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn session_output_lengths(session_dir: &Path) -> Result<(u64, u64), ProcessSessionError> {
    let stdout = std::fs::metadata(session_dir.join("stdout.log"))
        .map_err(io_error)?
        .len();
    let stderr = std::fs::metadata(session_dir.join("stderr.log"))
        .map_err(io_error)?
        .len();
    Ok((stdout, stderr))
}

fn file_modified_at(path: &Path) -> Result<DateTime<Utc>, ProcessSessionError> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(DateTime::<Utc>::from)
        .map_err(io_error)
}

fn finalize_governance_termination(
    session_dir: &Path,
    reason: ProcessSessionTerminationReason,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |manifest| {
        if !manifest.state.is_terminal() {
            manifest.state = ProcessSessionState::Terminated;
            manifest.resource_phase = terminal_resource_phase(&manifest.resource_identity);
            manifest.pid = None;
            manifest.process_group_id = None;
            manifest.exit_code = None;
            manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
            manifest.last_operation = "governance_terminated".into();
            manifest.last_input_digest = None;
            manifest.termination_reason = Some(reason);
            manifest.updated_at = Utc::now().max(manifest.updated_at);
        }
        Ok(())
    })
}

fn finalize_start_failure(
    session_dir: &Path,
    expected: &ProcessSessionManifest,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |manifest| {
        validate_same_manifest(manifest, expected)?;
        if manifest.state != ProcessSessionState::Starting
            || manifest.resource_phase != ProcessSessionResourcePhase::Prepared
        {
            return Err(ProcessSessionError::Conflict);
        }
        manifest.state = ProcessSessionState::Terminated;
        manifest.resource_phase = terminal_resource_phase(&manifest.resource_identity);
        manifest.pid = None;
        manifest.process_group_id = None;
        manifest.exit_code = None;
        manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
        manifest.last_operation = "start_failed".into();
        manifest.last_input_digest = None;
        manifest.termination_reason = Some(ProcessSessionTerminationReason::StartFailed);
        manifest.updated_at = Utc::now().max(manifest.updated_at);
        Ok(())
    })
}

fn mark_identity_indeterminate(
    session_dir: &Path,
    expected: &ProcessSessionManifest,
) -> Result<(), ProcessSessionError> {
    mutate_manifest(session_dir, |manifest| {
        validate_same_manifest(manifest, expected)?;
        manifest.state = ProcessSessionState::Indeterminate;
        manifest.resource_phase = terminal_resource_phase(&manifest.resource_identity);
        manifest.pid = None;
        manifest.process_group_id = None;
        manifest.exit_code = None;
        manifest.operation_sequence = manifest.operation_sequence.saturating_add(1);
        manifest.last_operation = "governance_identity_ambiguous".into();
        manifest.last_input_digest = None;
        manifest.termination_reason = None;
        manifest.updated_at = Utc::now().max(manifest.updated_at);
        Ok(())
    })
}

fn ensure_attached_identity(
    session_dir: &Path,
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<i32, ProcessSessionError> {
    let pgid = manifest
        .process_group_id
        .ok_or(ProcessSessionError::Indeterminate)?;
    if !identity_is_held(session_dir)? || !resource_identity_alive(manifest, resource_backend)? {
        return Err(ProcessSessionError::Indeterminate);
    }
    Ok(pgid)
}

fn cleanup_terminal_resource_identity(
    session_dir: &Path,
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<(), ProcessSessionError> {
    if !manifest.state.is_terminal() {
        return Err(ProcessSessionError::Indeterminate);
    }
    if manifest.resource_phase == ProcessSessionResourcePhase::Cleaned {
        return Ok(());
    }
    if manifest.resource_phase != ProcessSessionResourcePhase::CleanupPending {
        return Err(ProcessSessionError::Indeterminate);
    }
    remove_linux_cgroup_identity(
        manifest.session_id,
        &manifest.resource_identity,
        resource_backend,
    )?;
    mutate_manifest(session_dir, |current| {
        validate_same_manifest(current, manifest)?;
        current.resource_phase = ProcessSessionResourcePhase::Cleaned;
        current.operation_sequence = current.operation_sequence.saturating_add(1);
        current.last_operation = "resource_cleaned".into();
        current.updated_at = Utc::now().max(current.updated_at);
        Ok(())
    })
}

fn remove_linux_cgroup_identity(
    session_id: Uuid,
    resource_identity: &ProcessSessionResourceIdentity,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<(), ProcessSessionError> {
    match (resource_backend, resource_identity) {
        (ProcessSessionResourceBackend::UnixRlimit, ProcessSessionResourceIdentity::UnixRlimit) => {
            Ok(())
        }
        (
            ProcessSessionResourceBackend::LinuxCgroupV2 { root },
            ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
        ) if resource_identity.is_well_formed_for(session_id) => {
            remove_linux_cgroup_v2_group_root(root, group_name)
                .map_err(|_| ProcessSessionError::Indeterminate)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

fn read_chunk(
    path: &Path,
    cursor: u64,
    limit: usize,
) -> Result<(Vec<u8>, u64), ProcessSessionError> {
    let mut file = File::open(path).map_err(io_error)?;
    let length = file.metadata().map_err(io_error)?.len();
    if cursor > length {
        return Err(ProcessSessionError::InvalidCursor);
    }
    file.seek(SeekFrom::Start(cursor)).map_err(io_error)?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    let next = cursor
        .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .ok_or(ProcessSessionError::InvalidCursor)?;
    Ok((bytes, next))
}

fn read_tail(path: &Path, limit: usize) -> Result<(Vec<u8>, u64, u64, bool), ProcessSessionError> {
    let length = File::open(path)
        .map_err(io_error)?
        .metadata()
        .map_err(io_error)?
        .len();
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let start = length.saturating_sub(limit);
    let (bytes, next) = read_chunk(path, start, usize::try_from(limit).unwrap_or(usize::MAX))?;
    Ok((bytes, start, next, start > 0))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn governance_digest(
    governance: &ProcessSessionGovernance,
    capabilities: ProcessSessionResourceCapabilities,
) -> String {
    let resource_backend = match &governance.resource_backend {
        ProcessSessionResourceBackendConfig::UnixRlimit => serde_json::json!({
            "kind": "unix_rlimit",
        }),
        ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root } => {
            serde_json::json!({
                "kind": "linux_cgroup_v2",
                "delegated_root": delegated_root,
            })
        }
    };
    sha256(
        serde_json::json!({
            "schema_version": PROCESS_SESSION_SCHEMA_VERSION,
            "resource_backend": resource_backend,
            "max_active_sessions": governance.max_active_sessions,
            "max_active_sessions_per_tenant": governance.max_active_sessions_per_tenant,
            "max_active_sessions_per_workspace": governance.max_active_sessions_per_workspace,
            "max_runtime_millis": governance.max_runtime.as_millis().to_string(),
            "idle_timeout_millis": governance.idle_timeout.as_millis().to_string(),
            "max_output_bytes_per_stream": governance.max_output_bytes_per_stream,
            "max_cpu_seconds": governance.max_cpu_seconds,
            "max_memory_bytes": governance.max_memory_bytes,
            "max_processes": governance.max_processes,
            "require_whole_process_tree_accounting": governance.require_whole_process_tree_accounting,
            "resource_capabilities": {
                "backend": match capabilities.backend {
                    ProcessSessionResourceBackendKind::UnixRlimit => "unix_rlimit",
                    ProcessSessionResourceBackendKind::LinuxCgroupV2 => "linux_cgroup_v2",
                    ProcessSessionResourceBackendKind::Unsupported => "unsupported",
                },
                "hard_output_file_limit": capabilities.hard_output_file_limit,
                "hard_cpu_time_limit": capabilities.hard_cpu_time_limit,
                "hard_memory_limit": capabilities.hard_memory_limit,
                "hard_process_count_limit": capabilities.hard_process_count_limit,
                "whole_process_tree_accounting": capabilities.whole_process_tree_accounting,
            },
        })
        .to_string()
        .as_bytes(),
    )
}

fn resolve_resource_capabilities(
    backend: &ProcessSessionResourceBackendConfig,
) -> Result<ProcessSessionResourceCapabilities, ProcessSessionError> {
    match backend {
        ProcessSessionResourceBackendConfig::UnixRlimit => {
            Ok(ProcessSessionResourceCapabilities::current())
        }
        ProcessSessionResourceBackendConfig::LinuxCgroupV2 { .. } => {
            #[cfg(not(target_os = "linux"))]
            {
                Err(ProcessSessionError::UnsupportedPlatform)
            }
            #[cfg(target_os = "linux")]
            {
                Err(ProcessSessionError::UnsupportedResourceCapability(
                    "linux_cgroup_v2_backend_not_wired",
                ))
            }
        }
    }
}

fn manifest_digest(manifest: &ProcessSessionManifest) -> String {
    let bytes = serde_json::to_vec(manifest).expect("process session manifest is serializable");
    sha256(&bytes)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn io_error(error: std::io::Error) -> ProcessSessionError {
    ProcessSessionError::Io(error.to_string())
}

#[cfg(unix)]
fn open_pty(size: ProcessTerminalSize) -> Result<(File, File), ProcessSessionError> {
    use std::os::fd::FromRawFd;

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut dimensions = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes both output descriptors or returns an error;
    // null name/termios pointers request the platform defaults.
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dimensions,
        )
    };
    if result != 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: successful openpty returned two newly-owned descriptors.
    let master = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: successful openpty returned two newly-owned descriptors.
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    Ok((master, slave))
}

#[cfg(unix)]
fn install_controlling_terminal(
    command: &mut Command,
    slave: &File,
) -> Result<(), ProcessSessionError> {
    use std::os::fd::AsRawFd;

    let slave_fd = slave.as_raw_fd();
    // SAFETY: the closure runs after fork and before exec. setsid creates the
    // session/process group and TIOCSCTTY binds the already-open slave PTY as
    // its controlling terminal.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(unix)]
fn create_fifo(path: &Path) -> Result<(), ProcessSessionError> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ProcessSessionError::InvalidConfiguration("FIFO path contains NUL".into()))?;
    // SAFETY: `path` is a valid NUL-terminated pathname and the mode contains
    // only ordinary permission bits.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(io_error(std::io::Error::last_os_error()))
    }
}

#[cfg(not(unix))]
fn create_fifo(_path: &Path) -> Result<(), ProcessSessionError> {
    Err(ProcessSessionError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_fifo_read_write(path: &Path) -> Result<File, ProcessSessionError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(io_error)
}

#[cfg(unix)]
fn write_fifo(path: &Path, bytes: &[u8]) -> Result<(), ProcessSessionError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.flush().map_err(io_error)
}

#[cfg(unix)]
fn install_identity_lease(
    command: &mut Command,
    identity_path: &Path,
) -> Result<(), ProcessSessionError> {
    use std::os::unix::ffi::OsStrExt;
    let identity_path = CString::new(identity_path.as_os_str().as_bytes()).map_err(|_| {
        ProcessSessionError::InvalidConfiguration("identity path contains NUL".into())
    })?;
    // SAFETY: the closure runs after fork and before exec. It uses only libc
    // syscalls and a preallocated CString. Opening the lease in the child is
    // essential: clearing CLOEXEC on a parent-owned fd would let an unrelated
    // concurrently spawned Tool inherit and pin this session's identity.
    unsafe {
        command.pre_exec(move || {
            let fd = libc::open(identity_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(error);
            }
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(error);
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(unix)]
fn install_process_resource_limits(
    command: &mut Command,
    max_output_bytes_per_stream: u64,
    max_cpu_seconds: u64,
    max_memory_bytes: Option<u64>,
) -> Result<(), ProcessSessionError> {
    let output_limit = libc::rlim_t::try_from(max_output_bytes_per_stream).map_err(|_| {
        ProcessSessionError::InvalidConfiguration(
            "process output limit cannot be represented on this platform".into(),
        )
    })?;
    let cpu_limit = libc::rlim_t::try_from(max_cpu_seconds).map_err(|_| {
        ProcessSessionError::InvalidConfiguration(
            "process CPU limit cannot be represented on this platform".into(),
        )
    })?;
    #[cfg(not(target_os = "macos"))]
    let memory_limit = max_memory_bytes
        .map(libc::rlim_t::try_from)
        .transpose()
        .map_err(|_| {
            ProcessSessionError::InvalidConfiguration(
                "process memory limit cannot be represented on this platform".into(),
            )
        })?;
    #[cfg(target_os = "macos")]
    let _ = max_memory_bytes;
    // SAFETY: this closure runs in the child after fork and before exec. It
    // uses only the async-signal-safe `setrlimit` syscall and captured scalars.
    unsafe {
        command.pre_exec(move || {
            for (resource, limit) in [
                (libc::RLIMIT_FSIZE, output_limit),
                (libc::RLIMIT_CPU, cpu_limit),
            ] {
                let resource_limit = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &resource_limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            #[cfg(not(target_os = "macos"))]
            if let Some(memory_limit) = memory_limit {
                let memory_resource_limit = libc::rlimit {
                    rlim_cur: memory_limit,
                    rlim_max: memory_limit,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &memory_resource_limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(unix)]
fn identity_is_held(session_dir: &Path) -> Result<bool, ProcessSessionError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_dir.join("identity.lock"))
        .map_err(io_error)?;
    match lock_exclusive_nonblocking(&file) {
        Ok(()) => {
            unlock(&file)?;
            Ok(false)
        }
        Err(ProcessSessionError::Conflict) => Ok(true),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> Result<(), ProcessSessionError> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock operates on a valid owned file descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io_error(std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &File) -> Result<(), ProcessSessionError> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock operates on a valid owned file descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    let raw = error.raw_os_error();
    if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
        Err(ProcessSessionError::Conflict)
    } else {
        Err(io_error(error))
    }
}

#[cfg(unix)]
fn unlock(file: &File) -> Result<(), ProcessSessionError> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock operates on a valid owned file descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(io_error(std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
fn process_group_alive(process_group_id: i32) -> bool {
    if process_group_id <= 0 {
        return false;
    }
    // SAFETY: negative pid targets one process group and signal 0 performs no
    // mutation; it only checks whether the group exists and is signalable.
    if unsafe { libc::kill(-process_group_id, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn signal_group(process_group_id: i32, signal: i32) -> Result<(), ProcessSessionError> {
    if process_group_id <= 0 {
        return Err(ProcessSessionError::Indeterminate);
    }
    // SAFETY: negative pid targets only the recorded process group.
    if unsafe { libc::kill(-process_group_id, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io_error(error))
    }
}

#[cfg(unix)]
fn signal_group_with_identity_fence(
    session_dir: &Path,
    process_group_id: i32,
    signal: i32,
) -> Result<bool, ProcessSessionError> {
    if !identity_is_held(session_dir)? {
        return Ok(false);
    }
    if process_group_id <= 0 {
        return Err(ProcessSessionError::Indeterminate);
    }
    // SAFETY: negative pid targets only the recorded process group. The
    // inherited identity lease is checked immediately before signalling so a
    // released session cannot authorize acting on a recycled process group.
    if unsafe { libc::kill(-process_group_id, signal) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => identity_is_held(session_dir),
        Some(libc::EPERM) if !identity_is_held(session_dir)? => Ok(false),
        _ => Err(io_error(error)),
    }
}

#[cfg(unix)]
async fn terminate_resource_identity(
    session_dir: &Path,
    manifest: &ProcessSessionManifest,
    resource_backend: &ProcessSessionResourceBackend,
) -> Result<(), ProcessSessionError> {
    match (resource_backend, &manifest.resource_identity) {
        (ProcessSessionResourceBackend::UnixRlimit, ProcessSessionResourceIdentity::UnixRlimit) => {
            let process_group_id = manifest
                .process_group_id
                .ok_or(ProcessSessionError::Indeterminate)?;
            terminate_process_group(session_dir, process_group_id).await
        }
        (ProcessSessionResourceBackend::LinuxCgroupV2 { .. }, _) => {
            let group = open_linux_cgroup(resource_backend, manifest)?;
            kill_linux_cgroup_v2_group(&group).map_err(|_| ProcessSessionError::Indeterminate)?;
            let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
            while tokio::time::Instant::now() < deadline
                && read_linux_cgroup_populated_group(&group)
                    .map_err(|_| ProcessSessionError::Indeterminate)?
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if read_linux_cgroup_populated_group(&group)
                .map_err(|_| ProcessSessionError::Indeterminate)?
            {
                return Err(ProcessSessionError::Indeterminate);
            }
            for _ in 0..100 {
                if !identity_is_held(session_dir)? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(ProcessSessionError::Indeterminate)
        }
        _ => Err(ProcessSessionError::Indeterminate),
    }
}

#[cfg(unix)]
async fn terminate_process_group(
    session_dir: &Path,
    process_group_id: i32,
) -> Result<(), ProcessSessionError> {
    if !signal_group_with_identity_fence(session_dir, process_group_id, libc::SIGTERM)? {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
    while tokio::time::Instant::now() < deadline {
        if !identity_is_held(session_dir)? {
            return Ok(());
        }
        if !process_group_alive(process_group_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if identity_is_held(session_dir)?
        && process_group_alive(process_group_id)
        && !signal_group_with_identity_fence(session_dir, process_group_id, libc::SIGKILL)?
    {
        return Ok(());
    }
    // The inherited identity lock is released only after every descendant
    // that retained it has exited. Do not publish a terminal state first.
    for _ in 0..100 {
        if !identity_is_held(session_dir)? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(ProcessSessionError::Indeterminate)
}

fn open_append(path: &Path) -> Result<File, ProcessSessionError> {
    OpenOptions::new().append(true).open(path).map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TrustedNativeToolDefinition, WorkspaceAccess};
    use agent_protocol::{SandboxClass, ToolCall, ToolEffect};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    #[cfg(unix)]
    #[tokio::test]
    async fn released_identity_never_authorizes_signalling_a_reused_process_group() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("identity.lock"), b"").unwrap();

        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let process_group_id = child.id().unwrap() as i32;

        let result = terminate_process_group(root.path(), process_group_id).await;
        let was_still_running = child.try_wait().unwrap().is_none();

        if was_still_running {
            signal_group(process_group_id, libc::SIGKILL).unwrap();
        }
        let _ = child.wait().await;

        assert!(
            result.is_ok(),
            "released identity should be a no-op: {result:?}"
        );
        assert!(
            was_still_running,
            "an unlocked identity lease allowed a recycled process group to be killed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_process_group_cannot_hide_a_still_held_identity_lease() {
        let root = tempfile::tempdir().unwrap();
        let identity_path = root.path().join("identity.lock");
        std::fs::write(&identity_path, b"").unwrap();

        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30");
        install_identity_lease(&mut command, &identity_path).unwrap();
        let mut child = command.spawn().unwrap();
        let lease_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while !identity_is_held(root.path()).unwrap() {
            assert!(
                tokio::time::Instant::now() < lease_deadline,
                "child never acquired the identity lease"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let result = terminate_process_group(root.path(), i32::MAX).await;
        let was_still_running = child.try_wait().unwrap().is_none();
        child.kill().await.unwrap();
        let _ = child.wait().await;

        assert!(
            matches!(result, Err(ProcessSessionError::Indeterminate)),
            "a missing group hid a live identity lease: {result:?}"
        );
        assert!(was_still_running, "the lease holder exited unexpectedly");
    }

    #[tokio::test]
    async fn schema_four_unix_starting_remains_indeterminate_after_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        std::fs::create_dir(&session_dir).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        let now = Utc::now();
        let legacy = ProcessSessionManifest {
            schema_version: 4,
            session_id,
            tenant_id: Uuid::now_v7(),
            workspace_root: root.path().to_path_buf(),
            source_run_id: Uuid::now_v7(),
            source_attempt_id: Uuid::now_v7(),
            source_tool_call_id: "schema-four-unix-starting".into(),
            source_binding_digest: "0".repeat(64),
            implementation_digest: "1".repeat(64),
            governance_digest: "2".repeat(64),
            resource_identity: ProcessSessionResourceIdentity::UnixRlimit,
            resource_phase: ProcessSessionResourcePhase::Unprepared,
            state: ProcessSessionState::Starting,
            pid: None,
            process_group_id: None,
            exit_code: None,
            operation_sequence: 1,
            last_operation: "start_intent".into(),
            last_input_digest: None,
            recovery_count: 0,
            started_at: now,
            execution_deadline_at: now + chrono::Duration::hours(1),
            idle_timeout_millis: 60_000,
            last_activity_at: now,
            max_output_bytes_per_stream: 1024,
            max_cpu_seconds: 2,
            max_memory_bytes: None,
            observed_cpu_usage_micros: 0,
            observed_stdout_bytes: 0,
            observed_stderr_bytes: 0,
            termination_reason: None,
            updated_at: now,
        };
        let digest = manifest_digest(&legacy);
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&PersistedProcessSessionManifest {
                manifest: legacy,
                digest,
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &ProcessSessionResourceBackend::UnixRlimit)
                .await
                .unwrap(),
            ProcessSessionSweepOutcome::Indeterminate,
            "schema 4 Unix Starting did not record which side of spawn it reached"
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::Cleaned
        );
    }

    #[tokio::test]
    async fn unix_start_records_a_durable_prepared_transition() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let workspace_root = root.path().join("workspace");
        std::fs::create_dir(&state_root).unwrap();
        std::fs::create_dir(&workspace_root).unwrap();
        let executable = root.path().join("prepared-session");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'ready\\n'\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
            trusted_root: root.path().to_path_buf(),
            executable,
            fixed_args: Vec::new(),
            workspace_access: WorkspaceAccess::ReadWrite,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        })
        .unwrap();
        let manager = PersistentProcessSessionManager::new(state_root, executor, 1024).unwrap();
        let session_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let result = manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: ToolExecutionRequest {
                    call: ToolCall {
                        id: "unix-launch-prepared".into(),
                        name: PROCESS_START_TOOL.into(),
                        arguments: json!({}),
                    },
                    effect: ToolEffect::NonIdempotent,
                    sandbox: SandboxClass::TrustedNative,
                    binding_digest: "d".repeat(64),
                },
                context: ToolExecutionContext {
                    tenant_id,
                    application_id: Uuid::nil(),
                    workload_identity_id: Uuid::nil(),
                    run_id: Uuid::now_v7(),
                    session_id: Uuid::nil(),
                    workspace_id: Uuid::nil(),
                    agent_version_id: Uuid::nil(),
                    attempt_id: Uuid::now_v7(),
                    workspace_root: workspace_root.clone(),
                    timeout: Duration::from_secs(5),
                    cancellation: CancellationToken::new(),
                    requested_at: Utc::now(),
                },
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(result.state, ProcessSessionState::Running);
        let manifest = load_manifest(&manager.session_dir(session_id)).unwrap();
        assert_eq!(manifest.state, ProcessSessionState::Running);
        assert_eq!(manifest.resource_phase, ProcessSessionResourcePhase::Active);
        assert_eq!(
            manifest.operation_sequence, 3,
            "Running was published without a separate durable prepared transition"
        );
        assert_eq!(manifest.last_operation, "started");

        manager
            .interact(
                &ProcessSessionAccess {
                    tenant_id,
                    workspace_root,
                },
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: 0,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Close,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_start_executor_preserves_a_durable_typed_start_failure() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let workspace_root = root.path().join("workspace");
        std::fs::create_dir(&state_root).unwrap();
        std::fs::create_dir(&workspace_root).unwrap();
        let executable = root.path().join("start-failure-session");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'unexpected-start\\n'\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
            trusted_root: root.path().to_path_buf(),
            executable,
            fixed_args: Vec::new(),
            workspace_access: WorkspaceAccess::ReadWrite,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        })
        .unwrap();
        let manager =
            Arc::new(PersistentProcessSessionManager::new(state_root, executor, 1024).unwrap());
        let tenant_id = Uuid::now_v7();
        let start_executor = ProcessSessionToolExecutor::new(
            Arc::clone(&manager),
            ProcessSessionToolOperation::Start,
        );

        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let spawn_blocker = tokio::task::spawn_blocking(move || {
            let spawn_guard = PROCESS_SESSION_SPAWN_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap();
            let _ = locked_tx.send(());
            let _ = release_rx.recv();
            drop(spawn_guard);
        });
        locked_rx.await.expect("spawn boundary was not acquired");
        let starting_workspace = workspace_root.clone();
        let start_task = tokio::spawn(async move {
            start_executor
                .execute(
                    ToolExecutionRequest {
                        call: ToolCall {
                            id: "durable-start-failure".into(),
                            name: PROCESS_START_TOOL.into(),
                            arguments: json!({}),
                        },
                        effect: ToolEffect::NonIdempotent,
                        sandbox: SandboxClass::TrustedNative,
                        binding_digest: "d".repeat(64),
                    },
                    ToolExecutionContext {
                        tenant_id,
                        application_id: Uuid::nil(),
                        workload_identity_id: Uuid::nil(),
                        run_id: Uuid::now_v7(),
                        session_id: Uuid::nil(),
                        workspace_id: Uuid::nil(),
                        agent_version_id: Uuid::nil(),
                        attempt_id: Uuid::now_v7(),
                        workspace_root: starting_workspace,
                        timeout: Duration::from_secs(5),
                        cancellation: CancellationToken::new(),
                        requested_at: Utc::now(),
                    },
                )
                .await
        });

        let session_dir = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for entry in std::fs::read_dir(manager.sessions_root()).unwrap() {
                    let candidate = entry.unwrap().path();
                    if load_manifest(&candidate)
                        .is_ok_and(|manifest| manifest.last_operation == "launch_prepared")
                    {
                        return candidate;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("start never reached its durable prepared boundary");
        let session_id = load_manifest(&session_dir).unwrap().session_id;
        std::fs::remove_dir(&workspace_root).unwrap();
        release_tx.send(()).unwrap();
        spawn_blocker.await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), start_task)
            .await
            .expect("spawn failure did not return")
            .unwrap()
            .unwrap_err();
        let manifest = load_manifest(&session_dir).unwrap();
        assert_eq!(
            manifest.state,
            ProcessSessionState::Terminated,
            "a synchronous spawn failure remained ambiguous"
        );
        assert_eq!(
            manifest.resource_phase,
            ProcessSessionResourcePhase::Cleaned
        );
        assert_eq!(manifest.operation_sequence, 3);
        assert_eq!(manifest.last_operation, "start_failed");
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(session_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            persisted["manifest"]["termination_reason"],
            json!("start_failed")
        );
        assert!(
            matches!(
                &error,
                ToolExecutionError::ProcessSessionStartFailed {
                    session_id: failed_session_id,
                    reason,
                } if *failed_session_id == session_id && !reason.trim().is_empty()
            ),
            "the caller did not receive the failed Session identity: {error}"
        );
    }

    #[tokio::test]
    async fn schema_five_active_session_is_rewritten_by_replacement() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let workspace_root = root.path().join("workspace");
        std::fs::create_dir(&state_root).unwrap();
        std::fs::create_dir(&workspace_root).unwrap();
        let executable = root.path().join("schema-five-session");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'ready\\n'\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
            trusted_root: root.path().to_path_buf(),
            executable,
            fixed_args: Vec::new(),
            workspace_access: WorkspaceAccess::ReadWrite,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        })
        .unwrap();
        let first =
            PersistentProcessSessionManager::new(state_root.clone(), executor.clone(), 1024)
                .unwrap();
        let session_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        first
            .start(ProcessSessionStartRequest {
                session_id,
                request: ToolExecutionRequest {
                    call: ToolCall {
                        id: "schema-five-active".into(),
                        name: PROCESS_START_TOOL.into(),
                        arguments: json!({}),
                    },
                    effect: ToolEffect::NonIdempotent,
                    sandbox: SandboxClass::TrustedNative,
                    binding_digest: "d".repeat(64),
                },
                context: ToolExecutionContext {
                    tenant_id,
                    application_id: Uuid::nil(),
                    workload_identity_id: Uuid::nil(),
                    run_id: Uuid::now_v7(),
                    session_id: Uuid::nil(),
                    workspace_id: Uuid::nil(),
                    agent_version_id: Uuid::nil(),
                    attempt_id: Uuid::now_v7(),
                    workspace_root: workspace_root.clone(),
                    timeout: Duration::from_secs(5),
                    cancellation: CancellationToken::new(),
                    requested_at: Utc::now(),
                },
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap();
        let session_dir = first.session_dir(session_id);
        let mut legacy = load_manifest(&session_dir).unwrap();
        legacy.schema_version = 5;
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&PersistedProcessSessionManifest {
                digest: manifest_digest(&legacy),
                manifest: legacy,
            })
            .unwrap(),
        )
        .unwrap();

        let replacement = PersistentProcessSessionManager::new(state_root, executor, 1024).unwrap();
        assert_eq!(
            replacement
                .recover(
                    &ProcessSessionAccess {
                        tenant_id,
                        workspace_root: workspace_root.clone(),
                    },
                    session_id,
                )
                .await
                .unwrap(),
            ProcessSessionRecovery::Reattached
        );
        let migrated = load_manifest(&session_dir).unwrap();
        assert_eq!(
            migrated.schema_version, 7,
            "an active schema-5 record was not upgraded before schema-7 values can be written"
        );
        let mut schema_six = migrated;
        schema_six.schema_version = 6;
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&PersistedProcessSessionManifest {
                digest: manifest_digest(&schema_six),
                manifest: schema_six,
            })
            .unwrap(),
        )
        .unwrap();

        replacement
            .interact(
                &ProcessSessionAccess {
                    tenant_id,
                    workspace_root,
                },
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: 0,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Close,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            load_manifest(&session_dir).unwrap().schema_version,
            7,
            "a schema-6 record was not upgraded before the next durable mutation"
        );
    }

    #[tokio::test]
    async fn linux_start_rolls_back_the_group_and_never_spawns_when_preparation_fails() {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let sessions_root = state_root.join("process-sessions");
        let workspace_root = root.path().join("workspace");
        let delegated_root = root.path().join("cgroups");
        std::fs::create_dir_all(&sessions_root).unwrap();
        std::fs::create_dir(&workspace_root).unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        let marker = workspace_root.join("spawned");
        let executable = root.path().join("must-not-spawn");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf spawned > {}\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
            trusted_root: root.path().to_path_buf(),
            executable,
            fixed_args: Vec::new(),
            workspace_access: WorkspaceAccess::ReadWrite,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
        })
        .unwrap();
        let governance = ProcessSessionGovernance {
            resource_backend: ProcessSessionResourceBackendConfig::LinuxCgroupV2 {
                delegated_root: delegated_root.clone(),
            },
            max_memory_bytes: Some(268_435_456),
            max_processes: Some(17),
            require_whole_process_tree_accounting: true,
            ..ProcessSessionGovernance::default()
        };
        let resource_backend =
            ProcessSessionResourceBackend::open(&governance.resource_backend).unwrap();
        let manager = PersistentProcessSessionManager {
            state_root,
            executor,
            max_output_chunk_bytes: 1024,
            governance,
            resource_capabilities: ProcessSessionResourceCapabilities {
                backend: ProcessSessionResourceBackendKind::LinuxCgroupV2,
                hard_output_file_limit: true,
                hard_cpu_time_limit: true,
                hard_memory_limit: true,
                hard_process_count_limit: true,
                whole_process_tree_accounting: true,
            },
            resource_backend,
            pty_supervisor: None,
            wait_observation_metrics: Arc::new(ProcessWaitObservationMetrics::default()),
            wait_observers: Mutex::new(HashMap::new()),
        };
        let session_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let result = manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: ToolExecutionRequest {
                    call: ToolCall {
                        id: "linux-start-preparation".into(),
                        name: PROCESS_START_TOOL.into(),
                        arguments: json!({}),
                    },
                    effect: ToolEffect::NonIdempotent,
                    sandbox: SandboxClass::TrustedNative,
                    binding_digest: "d".repeat(64),
                },
                context: ToolExecutionContext {
                    tenant_id,
                    application_id: Uuid::nil(),
                    workload_identity_id: Uuid::nil(),
                    run_id: Uuid::now_v7(),
                    session_id: Uuid::nil(),
                    workspace_id: Uuid::nil(),
                    agent_version_id: Uuid::nil(),
                    attempt_id: Uuid::now_v7(),
                    workspace_root,
                    timeout: Duration::from_secs(5),
                    cancellation: CancellationToken::new(),
                    requested_at: Utc::now(),
                },
                initial_stdin: Vec::new(),
            })
            .await;

        assert!(
            matches!(result, Err(ProcessSessionError::InvalidConfiguration(_))),
            "Linux start bypassed cgroup preparation: {result:?}"
        );
        assert!(!marker.exists(), "the child ran before cgroup preparation");
        assert!(
            !delegated_root
                .join(format!("session-{session_id}"))
                .exists(),
            "failed preparation leaked the new cgroup"
        );
    }

    #[tokio::test]
    async fn terminal_sweep_retries_cleanup_on_the_manager_owned_cgroup_root() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let moved_root = root.path().join("cgroups-original");
        let group_name = format!("session-{session_id}");
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::write(session_dir.join("control.lock"), b"").unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        std::fs::create_dir(delegated_root.join(&group_name)).unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 {
                delegated_root: delegated_root.clone(),
            },
        )
        .unwrap();
        std::fs::rename(&delegated_root, &moved_root).unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        std::fs::create_dir(delegated_root.join(&group_name)).unwrap();
        let now = Utc::now();
        persist_manifest(
            &session_dir,
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id,
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "terminal-cleanup".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 {
                    group_name: group_name.clone(),
                },
                resource_phase: ProcessSessionResourcePhase::CleanupPending,
                state: ProcessSessionState::Exited,
                pid: None,
                process_group_id: None,
                exit_code: Some(0),
                operation_sequence: 2,
                last_operation: "exited".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Terminal
        );
        assert!(
            !moved_root.join(&group_name).exists(),
            "terminal sweep did not retry cleanup under the manager root"
        );
        assert!(
            delegated_root.join(&group_name).exists(),
            "terminal sweep escaped to the replacement configured path"
        );
    }

    #[tokio::test]
    async fn schema_three_starting_without_a_group_remains_indeterminate() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        let now = Utc::now();
        let legacy = LegacyProcessSessionManifestV3 {
            schema_version: 3,
            session_id,
            tenant_id: Uuid::now_v7(),
            workspace_root: root.path().to_path_buf(),
            source_run_id: Uuid::now_v7(),
            source_attempt_id: Uuid::now_v7(),
            source_tool_call_id: "legacy-starting-without-group".into(),
            source_binding_digest: "0".repeat(64),
            implementation_digest: "1".repeat(64),
            governance_digest: "2".repeat(64),
            resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 {
                group_name: format!("session-{session_id}"),
            },
            state: ProcessSessionState::Starting,
            pid: None,
            process_group_id: None,
            exit_code: None,
            operation_sequence: 1,
            last_operation: "start_intent".into(),
            last_input_digest: None,
            recovery_count: 0,
            started_at: now,
            execution_deadline_at: now + chrono::Duration::hours(1),
            idle_timeout_millis: 60_000,
            last_activity_at: now,
            max_output_bytes_per_stream: 1024,
            max_cpu_seconds: 2,
            max_memory_bytes: None,
            observed_cpu_usage_micros: 0,
            observed_stdout_bytes: 0,
            observed_stderr_bytes: 0,
            termination_reason: None,
            updated_at: now,
        };
        let digest = sha256(&serde_json::to_vec(&legacy).unwrap());
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&PersistedLegacyProcessSessionManifestV3 {
                manifest: legacy,
                digest,
            })
            .unwrap(),
        )
        .unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Indeterminate,
            "schema 3 cannot prove whether the Tool ran before the group disappeared"
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.schema_version, PROCESS_SESSION_SCHEMA_VERSION);
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::CleanupPending
        );
    }

    #[tokio::test]
    async fn schema_two_starting_without_an_identity_remains_indeterminate() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        std::fs::create_dir(&session_dir).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        let now = Utc::now();
        let legacy = LegacyProcessSessionManifestV2 {
            schema_version: 2,
            session_id,
            tenant_id: Uuid::now_v7(),
            workspace_root: root.path().to_path_buf(),
            source_run_id: Uuid::now_v7(),
            source_attempt_id: Uuid::now_v7(),
            source_tool_call_id: "schema-two-starting".into(),
            source_binding_digest: "0".repeat(64),
            implementation_digest: "1".repeat(64),
            governance_digest: "2".repeat(64),
            state: ProcessSessionState::Starting,
            pid: None,
            process_group_id: None,
            exit_code: None,
            operation_sequence: 1,
            last_operation: "start_intent".into(),
            last_input_digest: None,
            recovery_count: 0,
            started_at: now,
            execution_deadline_at: now + chrono::Duration::hours(1),
            idle_timeout_millis: 60_000,
            last_activity_at: now,
            max_output_bytes_per_stream: 1024,
            max_cpu_seconds: 2,
            max_memory_bytes: None,
            observed_stdout_bytes: 0,
            observed_stderr_bytes: 0,
            termination_reason: None,
            updated_at: now,
        };
        let digest = sha256(&serde_json::to_vec(&legacy).unwrap());
        std::fs::write(
            session_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&PersistedLegacyProcessSessionManifestV2 {
                manifest: legacy,
                digest,
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &ProcessSessionResourceBackend::UnixRlimit)
                .await
                .unwrap(),
            ProcessSessionSweepOutcome::Indeterminate,
            "schema 2 cannot prove that the pre-Running process never executed"
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.schema_version, PROCESS_SESSION_SCHEMA_VERSION);
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::Cleaned
        );
    }

    #[tokio::test]
    async fn prepared_starting_with_an_empty_group_remains_indeterminate() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let group_name = format!("session-{session_id}");
        let group = delegated_root.join(&group_name);
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::create_dir_all(&group).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        std::fs::write(group.join("cgroup.events"), b"populated 0\n").unwrap();
        std::fs::write(group.join("cgroup.kill"), b"").unwrap();
        let now = Utc::now();
        persist_manifest(
            &session_dir,
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id,
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "prepared-starting-empty-group".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 {
                    group_name: group_name.clone(),
                },
                resource_phase: ProcessSessionResourcePhase::Prepared,
                state: ProcessSessionState::Starting,
                pid: None,
                process_group_id: None,
                exit_code: None,
                operation_sequence: 2,
                last_operation: "resource_prepared".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Indeterminate,
            "an empty prepared group does not prove that a fast Tool never ran"
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::CleanupPending
        );
        assert_eq!(
            std::fs::read_to_string(group.join("cgroup.kill")).unwrap(),
            "1\n"
        );
    }

    #[tokio::test]
    async fn starting_intent_without_cgroup_or_identity_recovers_as_terminated() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        let now = Utc::now();
        persist_manifest(
            &session_dir,
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id,
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "starting-without-group".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 {
                    group_name: format!("session-{session_id}"),
                },
                resource_phase: ProcessSessionResourcePhase::Unprepared,
                state: ProcessSessionState::Starting,
                pid: None,
                process_group_id: None,
                exit_code: None,
                operation_sequence: 1,
                last_operation: "start_intent".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Terminated
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.state, ProcessSessionState::Terminated);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::Cleaned
        );
        assert_eq!(
            recovered.termination_reason,
            Some(ProcessSessionTerminationReason::RecoveredMissing)
        );
    }

    #[tokio::test]
    async fn starting_intent_with_populated_cgroup_is_killed_and_marked_indeterminate() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let group_name = format!("session-{session_id}");
        let group = delegated_root.join(&group_name);
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::create_dir_all(&group).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        std::fs::write(group.join("cgroup.events"), b"populated 1\n").unwrap();
        std::fs::write(group.join("cgroup.kill"), b"").unwrap();
        let now = Utc::now();
        persist_manifest(
            &session_dir,
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id,
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "starting-with-populated-group".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 {
                    group_name: group_name.clone(),
                },
                resource_phase: ProcessSessionResourcePhase::Prepared,
                state: ProcessSessionState::Starting,
                pid: None,
                process_group_id: None,
                exit_code: None,
                operation_sequence: 2,
                last_operation: "resource_prepared".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Indeterminate
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::CleanupPending
        );
        assert_eq!(
            std::fs::read_to_string(group.join("cgroup.kill")).unwrap(),
            "1\n",
            "an unpublished child was left running in the prepared cgroup"
        );
    }

    #[tokio::test]
    async fn starting_intent_with_ambiguous_controller_is_persistently_indeterminate() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let group_name = format!("session-{session_id}");
        let group = delegated_root.join(&group_name);
        std::fs::create_dir(&session_dir).unwrap();
        std::fs::create_dir_all(&group).unwrap();
        for name in ["stdout.log", "stderr.log", "identity.lock", "control.lock"] {
            std::fs::write(session_dir.join(name), b"").unwrap();
        }
        std::fs::write(group.join("cgroup.events"), b"populated maybe\n").unwrap();
        std::fs::write(group.join("cgroup.kill"), b"").unwrap();
        let now = Utc::now();
        persist_manifest(
            &session_dir,
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id,
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "starting-with-ambiguous-controller".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
                resource_phase: ProcessSessionResourcePhase::Prepared,
                state: ProcessSessionState::Starting,
                pid: None,
                process_group_id: None,
                exit_code: None,
                operation_sequence: 2,
                last_operation: "resource_prepared".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();

        assert_eq!(
            sweep_process_session(&session_dir, &backend).await.unwrap(),
            ProcessSessionSweepOutcome::Indeterminate
        );
        let recovered = load_manifest(&session_dir).unwrap();
        assert_eq!(recovered.state, ProcessSessionState::Indeterminate);
        assert_eq!(
            recovered.resource_phase,
            ProcessSessionResourcePhase::CleanupPending
        );
        assert_eq!(
            std::fs::read_to_string(group.join("cgroup.kill")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn unix_terminal_manifest_cannot_claim_cleanup_is_pending() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let error = persist_manifest(
            root.path(),
            &ProcessSessionManifest {
                schema_version: PROCESS_SESSION_SCHEMA_VERSION,
                session_id: Uuid::now_v7(),
                tenant_id: Uuid::now_v7(),
                workspace_root: root.path().to_path_buf(),
                source_run_id: Uuid::now_v7(),
                source_attempt_id: Uuid::now_v7(),
                source_tool_call_id: "invalid-unix-cleanup".into(),
                source_binding_digest: "0".repeat(64),
                implementation_digest: "1".repeat(64),
                governance_digest: "2".repeat(64),
                resource_identity: ProcessSessionResourceIdentity::UnixRlimit,
                resource_phase: ProcessSessionResourcePhase::CleanupPending,
                state: ProcessSessionState::Exited,
                pid: None,
                process_group_id: None,
                exit_code: Some(0),
                operation_sequence: 2,
                last_operation: "exited".into(),
                last_input_digest: None,
                recovery_count: 0,
                started_at: now,
                execution_deadline_at: now + chrono::Duration::hours(1),
                idle_timeout_millis: 60_000,
                last_activity_at: now,
                max_output_bytes_per_stream: 1024,
                max_cpu_seconds: 2,
                max_memory_bytes: None,
                observed_cpu_usage_micros: 0,
                observed_stdout_bytes: 0,
                observed_stderr_bytes: 0,
                termination_reason: None,
                updated_at: now,
            },
        )
        .expect_err("a Unix terminal manifest claimed Linux-style pending cleanup");

        assert!(matches!(error, ProcessSessionError::InvalidRequest(_)));
    }

    #[test]
    fn cgroup_cpu_observation_drives_whole_tree_cpu_limit() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let group_name = format!("session-{session_id}");
        let group = delegated_root.join(&group_name);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(session_dir.join("stdout.log"), b"").unwrap();
        std::fs::write(session_dir.join("stderr.log"), b"").unwrap();
        std::fs::write(session_dir.join("control.lock"), b"").unwrap();
        std::fs::write(
            group.join("cpu.stat"),
            b"usage_usec 2000000\nuser_usec 1500000\nsystem_usec 500000\n",
        )
        .unwrap();
        let now = Utc::now();
        let manifest = ProcessSessionManifest {
            schema_version: PROCESS_SESSION_SCHEMA_VERSION,
            session_id,
            tenant_id: Uuid::now_v7(),
            workspace_root: root.path().to_path_buf(),
            source_run_id: Uuid::now_v7(),
            source_attempt_id: Uuid::now_v7(),
            source_tool_call_id: "cpu-limit".into(),
            source_binding_digest: "0".repeat(64),
            implementation_digest: "1".repeat(64),
            governance_digest: "2".repeat(64),
            resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
            resource_phase: ProcessSessionResourcePhase::Active,
            state: ProcessSessionState::Running,
            pid: Some(1),
            process_group_id: Some(1),
            exit_code: None,
            operation_sequence: 1,
            last_operation: "started".into(),
            last_input_digest: None,
            recovery_count: 0,
            started_at: now,
            execution_deadline_at: now + chrono::Duration::hours(1),
            idle_timeout_millis: 60_000,
            last_activity_at: now,
            max_output_bytes_per_stream: 1024,
            max_cpu_seconds: 2,
            max_memory_bytes: None,
            observed_cpu_usage_micros: 0,
            observed_stdout_bytes: 0,
            observed_stderr_bytes: 0,
            termination_reason: None,
            updated_at: now,
        };
        persist_manifest(&session_dir, &manifest).unwrap();

        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 { delegated_root },
        )
        .unwrap();
        let refreshed = refresh_process_activity_with_resources(&session_dir, &backend).unwrap();

        assert_eq!(refreshed.observed_cpu_usage_micros, 2_000_000);
        assert_eq!(
            governance_termination_reason(&refreshed, now).unwrap(),
            Some(ProcessSessionTerminationReason::CpuLimit)
        );
    }

    #[test]
    fn opened_backend_keeps_later_sweeps_on_the_original_delegated_root() {
        let root = tempfile::tempdir().unwrap();
        let session_id = Uuid::now_v7();
        let session_dir = root.path().join("session-state");
        let delegated_root = root.path().join("cgroups");
        let moved_root = root.path().join("cgroups-original");
        let group_name = format!("session-{session_id}");
        let original_group = delegated_root.join(&group_name);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&original_group).unwrap();
        std::fs::write(session_dir.join("stdout.log"), b"").unwrap();
        std::fs::write(session_dir.join("stderr.log"), b"").unwrap();
        std::fs::write(session_dir.join("control.lock"), b"").unwrap();
        std::fs::write(original_group.join("cpu.stat"), b"usage_usec 2000000\n").unwrap();
        let backend = ProcessSessionResourceBackend::open(
            &ProcessSessionResourceBackendConfig::LinuxCgroupV2 {
                delegated_root: delegated_root.clone(),
            },
        )
        .unwrap();

        std::fs::rename(&delegated_root, &moved_root).unwrap();
        let replacement_group = delegated_root.join(&group_name);
        std::fs::create_dir_all(&replacement_group).unwrap();
        std::fs::write(replacement_group.join("cpu.stat"), b"usage_usec 7\n").unwrap();

        let now = Utc::now();
        let manifest = ProcessSessionManifest {
            schema_version: PROCESS_SESSION_SCHEMA_VERSION,
            session_id,
            tenant_id: Uuid::now_v7(),
            workspace_root: root.path().to_path_buf(),
            source_run_id: Uuid::now_v7(),
            source_attempt_id: Uuid::now_v7(),
            source_tool_call_id: "manager-root-pin".into(),
            source_binding_digest: "0".repeat(64),
            implementation_digest: "1".repeat(64),
            governance_digest: "2".repeat(64),
            resource_identity: ProcessSessionResourceIdentity::LinuxCgroupV2 { group_name },
            resource_phase: ProcessSessionResourcePhase::Active,
            state: ProcessSessionState::Running,
            pid: Some(1),
            process_group_id: Some(1),
            exit_code: None,
            operation_sequence: 1,
            last_operation: "started".into(),
            last_input_digest: None,
            recovery_count: 0,
            started_at: now,
            execution_deadline_at: now + chrono::Duration::hours(1),
            idle_timeout_millis: 60_000,
            last_activity_at: now,
            max_output_bytes_per_stream: 1024,
            max_cpu_seconds: 2,
            max_memory_bytes: None,
            observed_cpu_usage_micros: 0,
            observed_stdout_bytes: 0,
            observed_stderr_bytes: 0,
            termination_reason: None,
            updated_at: now,
        };
        persist_manifest(&session_dir, &manifest).unwrap();

        let refreshed = refresh_process_activity_with_resources(&session_dir, &backend).unwrap();

        assert_eq!(
            refreshed.observed_cpu_usage_micros, 2_000_000,
            "a later sweep escaped to the replacement delegated root"
        );
    }
}
