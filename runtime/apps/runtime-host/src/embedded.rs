//! Transport-neutral multi-tenant embedding surface.
//!
//! A Java SDK, sidecar, edge node, or desktop process can authenticate a caller
//! outside this crate and then select one pre-registered invocation profile.
//! Requests never carry filesystem paths or provider credentials, so an
//! untrusted client cannot turn identity fields into access to another
//! Workspace.

use crate::admission::{
    RuntimeAdmissionController, RuntimeAdmissionError, RuntimeAdmissionLimits,
    RuntimeAdmissionPermit, RuntimeAdmissionSnapshot,
};
use crate::retention::{
    RuntimeRetentionPolicy, RuntimeRetentionReport, RuntimeTerminalTombstone,
    available_tombstone_capacity, commit_retention_candidates, count_run_directories,
    ledger_counts_and_bytes, load_run_tombstone_index, read_retired_control,
    repair_committed_tombstones, run_binding_digest, scan_retention_candidates,
};
use crate::{
    LOCAL_EVENT_LOG_LINE_MAX_BYTES, LocalApprovalDecision, LocalApprovalResolution, LocalEvent,
    LocalMcpInputResolution, LocalResumeResolution, LocalRunOutcome, LocalRunRecord, LocalRunState,
    LocalRuntimeConfig, LocalRuntimeError, LocalRuntimeHost,
};
use agent_protocol::{McpInputResponse, RunStatus, RuntimeInvocationContext};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_EVENT_CURSOR_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_EVENT_CURSOR_MAX_EVENTS: usize = 256;
pub const EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY: usize = 256;
pub const EMBEDDED_EVENT_SUBSCRIPTION_MAX_ACTIVE: usize = 256;
pub const EMBEDDED_EVENT_SUBSCRIPTION_MAX_BUFFERED_EVENTS: usize = 1_024;
const EMBEDDED_EVENT_LOG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEventCursorRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub after_sequence: u64,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RuntimeEventCursorState {
    Running,
    Cancelling,
    WaitingApproval,
    Suspended,
    Interrupted,
    Terminal {
        status: RunStatus,
    },
    Retired {
        status: RunStatus,
        terminal_event_id: Uuid,
        terminal_sequence: u64,
        terminal_event_digest: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEventCursorPage {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub requested_after_sequence: u64,
    pub next_after_sequence: u64,
    pub earliest_available_sequence: Option<u64>,
    pub highest_committed_sequence: u64,
    pub history_gap: bool,
    pub has_more: bool,
    pub state: RuntimeEventCursorState,
    pub events: Vec<LocalEvent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventCursorErrorCode {
    UnsupportedSchema,
    InvalidRequest,
    NotFound,
    CursorAhead,
    IdentityMismatch,
    CorruptLog,
    StorageUnavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("Runtime event cursor {code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct RuntimeEventCursorError {
    pub code: RuntimeEventCursorErrorCode,
    pub message: String,
}

impl RuntimeEventCursorError {
    fn new(code: RuntimeEventCursorErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Transport-neutral command accepted by an embedded Runtime. Authentication,
/// signature verification and user-facing authorization belong to the Java,
/// CLI or desktop adapter; the Runtime still binds every command to the exact
/// immutable invocation and durable Run generation it is allowed to change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeControlCommand {
    pub schema_version: u32,
    pub command_id: Uuid,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub expected_owner_epoch: u64,
    pub action: RuntimeControlAction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuntimeControlAction {
    Resume,
    DecideApproval {
        target_run_id: Uuid,
        approval_id: Uuid,
        binding_digest: String,
        decision: LocalApprovalDecision,
    },
    Cancel {
        reason: String,
    },
    ResolveMcpInput {
        input_id: Uuid,
        input_version: u32,
        binding_digest: String,
        responses: std::collections::BTreeMap<String, McpInputResponse>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeControlReceiptState {
    Accepted,
    Completed,
}

/// Durable idempotency and audit evidence for one control command. The command
/// digest prevents a caller from reusing an id for a different decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeControlReceipt {
    pub schema_version: u32,
    pub command_id: Uuid,
    pub command_digest: String,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub expected_owner_epoch: u64,
    pub action: RuntimeControlAction,
    pub state: RuntimeControlReceiptState,
    pub applied_owner_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_status: Option<RunStatus>,
}

impl RuntimeControlReceipt {
    #[must_use]
    pub fn command(&self) -> RuntimeControlCommand {
        RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: self.command_id,
            invocation: self.invocation,
            run_id: self.run_id,
            expected_owner_epoch: self.expected_owner_epoch,
            action: self.action.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeControlResult {
    pub receipt: RuntimeControlReceipt,
    pub outcome: Option<LocalRunOutcome>,
}

/// Read-only process-local evidence for capacity and leak checks. It exposes
/// counts, never tenant identities, filesystem paths or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmbeddedRuntimeSnapshot {
    pub registered_profiles: usize,
    pub active_execution_owners: usize,
    pub peak_active_execution_owners: usize,
    pub active_event_subscriptions: usize,
    pub buffered_event_slots: usize,
    pub peak_active_event_subscriptions: usize,
    pub peak_buffered_event_slots: usize,
    pub admission: RuntimeAdmissionSnapshot,
}

/// One immutable invocation whose startup reconciliation could not be planned
/// or durably accepted. The error remains typed for an in-process operator;
/// network adapters must continue mapping it without leaking local paths.
#[derive(Debug)]
pub struct EmbeddedRuntimeProfileRecoveryFailure {
    pub invocation: RuntimeInvocationContext,
    pub error: EmbeddedRuntimeError,
}

/// Aggregate startup-recovery evidence for a multi-profile Runtime.
///
/// `recovered_runs` counts Run groups whose durable control commands were
/// accepted. A failed profile is isolated: its remaining plans are not issued,
/// while other registered profiles continue through the shared fair admission
/// controller.
#[derive(Debug)]
pub struct EmbeddedRuntimeRecoveryReport {
    pub scanned_profiles: usize,
    pub recovered_runs: usize,
    pub failures: Vec<EmbeddedRuntimeProfileRecoveryFailure>,
}

struct PlannedRunRecovery {
    commands: Vec<RuntimeControlCommand>,
}

#[derive(Default)]
struct EventSubscriptionCapacityState {
    active: usize,
    buffered_slots: usize,
    peak_active: usize,
    peak_buffered_slots: usize,
}

#[derive(Default)]
struct EventSubscriptionCapacity {
    state: Mutex<EventSubscriptionCapacityState>,
}

impl EventSubscriptionCapacity {
    fn reserve(
        self: &Arc<Self>,
        capacity: usize,
    ) -> Result<EventSubscriptionPermit, EmbeddedRuntimeError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active >= EMBEDDED_EVENT_SUBSCRIPTION_MAX_ACTIVE
            || state.buffered_slots.saturating_add(capacity)
                > EMBEDDED_EVENT_SUBSCRIPTION_MAX_BUFFERED_EVENTS
        {
            return Err(EmbeddedRuntimeError::Configuration(
                "event subscription process capacity is exhausted".into(),
            ));
        }
        state.active += 1;
        state.buffered_slots += capacity;
        state.peak_active = state.peak_active.max(state.active);
        state.peak_buffered_slots = state.peak_buffered_slots.max(state.buffered_slots);
        Ok(EventSubscriptionPermit {
            capacity: Arc::clone(self),
            buffered_slots: capacity,
        })
    }

    fn snapshot(&self) -> (usize, usize, usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.active,
            state.buffered_slots,
            state.peak_active,
            state.peak_buffered_slots,
        )
    }
}

struct EventSubscriptionPermit {
    capacity: Arc<EventSubscriptionCapacity>,
    buffered_slots: usize,
}

impl Drop for EventSubscriptionPermit {
    fn drop(&mut self) {
        let mut state = self
            .capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        state.buffered_slots = state.buffered_slots.saturating_sub(self.buffered_slots);
    }
}

/// A bounded, durable-cursor event stream for Java, CLI and future GUI
/// adapters. The producer reads the committed JSONL log and waits when this
/// channel is full, so a slow client cannot grow Runtime memory or block the
/// Agent execution that owns the log.
pub struct EmbeddedEventSubscription {
    receiver: mpsc::Receiver<Result<RuntimeEventStreamItem, EmbeddedRuntimeError>>,
    stop: CancellationToken,
    task: JoinHandle<()>,
    _permit: EventSubscriptionPermit,
}

impl EmbeddedEventSubscription {
    pub async fn recv(&mut self) -> Option<Result<RuntimeEventStreamItem, EmbeddedRuntimeError>> {
        self.receiver.recv().await
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.receiver.max_capacity()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuntimeEventStreamItem {
    Event {
        schema_version: u32,
        event: Box<LocalEvent>,
    },
    Boundary {
        schema_version: u32,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        next_after_sequence: u64,
        earliest_available_sequence: Option<u64>,
        highest_committed_sequence: u64,
        history_gap: bool,
        state: RuntimeEventCursorState,
    },
}

impl Drop for EmbeddedEventSubscription {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

struct ActiveExecution {
    cancellation: CancellationToken,
    finalizing: AtomicBool,
    record_gate: Mutex<()>,
    cancellation_commands: Mutex<Vec<Uuid>>,
}

type ActiveExecutionKey = (RuntimeInvocationContext, Uuid);
type ActiveExecutionMap = Arc<Mutex<HashMap<ActiveExecutionKey, Arc<ActiveExecution>>>>;

struct StateRootLease {
    #[cfg(unix)]
    file: std::fs::File,
}

impl StateRootLease {
    fn acquire(state_root: &Path) -> Result<Self, EmbeddedRuntimeError> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let path = state_root.join("runtime-state.lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)
                .map_err(|error| EmbeddedRuntimeError::Configuration(error.to_string()))?;
            // SAFETY: flock operates on a valid file descriptor owned by this
            // guard. The OS releases it after process death or normal Drop.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(EmbeddedRuntimeError::Configuration(
                    "Workspace state root already has another Runtime owner".into(),
                ));
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
impl Drop for StateRootLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor remains valid for the lifetime of the guard.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct ActiveExecutionGuard {
    active: ActiveExecutionMap,
    key: ActiveExecutionKey,
    execution: Arc<ActiveExecution>,
}

impl Drop for ActiveExecutionGuard {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.execution))
        {
            active.remove(&self.key);
        }
    }
}

enum RecordedOperation {
    Execute,
    Resume,
    Approval(LocalApprovalResolution),
    McpInput(LocalMcpInputResolution),
}

#[derive(Clone, Debug)]
pub struct RuntimeProfile {
    pub invocation: RuntimeInvocationContext,
    pub config: LocalRuntimeConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EmbeddedRuntimeError {
    #[error("embedded Runtime configuration is invalid: {0}")]
    Configuration(String),
    /// The caller supplied a structurally valid transport document whose
    /// command identity, version, or bounded action is invalid.
    #[error("Runtime control command is invalid: {0}")]
    InvalidControlCommand(String),
    #[error("Runtime invocation is not registered")]
    UnregisteredInvocation,
    /// A control command id is already bound to a different command.
    ///
    /// Its own variant rather than a `Configuration` string because it is the
    /// one failure here a caller can act on: it did not mistype configuration,
    /// it reused an idempotency key. An adapter needs to be able to say so.
    #[error("control command id was already used for another command")]
    ControlCommandRebound,
    #[error(transparent)]
    Admission(#[from] RuntimeAdmissionError),
    #[error(transparent)]
    EventCursor(#[from] RuntimeEventCursorError),
    #[error(transparent)]
    Runtime(#[from] LocalRuntimeError),
}

/// One process can host many immutable tenant/application/workspace profiles,
/// while the admission controller applies a shared global ceiling and a tenant
/// ceiling before constructing a Host.
pub struct EmbeddedRuntime {
    profiles: HashMap<RuntimeInvocationContext, LocalRuntimeConfig>,
    admission: Arc<RuntimeAdmissionController>,
    active: ActiveExecutionMap,
    recovery_gate: AsyncMutex<()>,
    peak_active_execution_owners: AtomicUsize,
    event_subscriptions: Arc<EventSubscriptionCapacity>,
    retention_policy: RuntimeRetentionPolicy,
    retention_gates: HashMap<PathBuf, Arc<Mutex<()>>>,
    retired_runs: HashMap<PathBuf, Arc<Mutex<HashMap<Uuid, RuntimeTerminalTombstone>>>>,
    tenant_retention_gates: HashMap<Uuid, Arc<Mutex<()>>>,
    _state_root_leases: Vec<StateRootLease>,
}

impl EmbeddedRuntime {
    pub fn new(
        limits: RuntimeAdmissionLimits,
        profiles: Vec<RuntimeProfile>,
    ) -> Result<Self, EmbeddedRuntimeError> {
        Self::new_with_retention(limits, profiles, RuntimeRetentionPolicy::default())
    }

    pub fn new_with_retention(
        limits: RuntimeAdmissionLimits,
        profiles: Vec<RuntimeProfile>,
        retention_policy: RuntimeRetentionPolicy,
    ) -> Result<Self, EmbeddedRuntimeError> {
        if profiles.is_empty() {
            return Err(EmbeddedRuntimeError::Configuration(
                "at least one invocation profile is required".into(),
            ));
        }
        let retention_policy = retention_policy.validate()?;
        let admission = Arc::new(RuntimeAdmissionController::new(limits)?);
        let mut by_identity = HashMap::with_capacity(profiles.len());
        let mut boundaries = HashMap::<(Uuid, Uuid, Uuid), (PathBuf, PathBuf)>::new();
        let mut state_root_owners = HashMap::<PathBuf, (Uuid, Uuid, Uuid)>::new();
        let mut workspace_root_owners = HashMap::<PathBuf, (Uuid, Uuid, Uuid)>::new();
        for mut profile in profiles {
            profile.invocation.validate().map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "invocation profile is invalid: {error}"
                ))
            })?;
            if !profile.config.state_root.is_absolute() {
                return Err(EmbeddedRuntimeError::Configuration(
                    "each profile state root must be absolute".into(),
                ));
            }
            std::fs::create_dir_all(&profile.config.state_root).map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "each profile state root must be creatable: {error}"
                ))
            })?;
            let state_root =
                std::fs::canonicalize(&profile.config.state_root).map_err(|error| {
                    EmbeddedRuntimeError::Configuration(format!(
                        "each profile state root must resolve to a real directory: {error}"
                    ))
                })?;
            let state_metadata = std::fs::metadata(&state_root).map_err(|error| {
                EmbeddedRuntimeError::Configuration(format!(
                    "each profile state root must be inspectable: {error}"
                ))
            })?;
            if !state_metadata.is_dir() {
                return Err(EmbeddedRuntimeError::Configuration(
                    "each profile state root must be a directory".into(),
                ));
            }
            profile.config.state_root = state_root;
            let workspace =
                std::fs::canonicalize(&profile.config.workspace_root).map_err(|error| {
                    EmbeddedRuntimeError::Configuration(format!(
                        "workspace root cannot be resolved: {error}"
                    ))
                })?;
            let boundary = (
                profile.invocation.tenant_id,
                profile.invocation.application_id,
                profile.invocation.workspace_id,
            );
            let roots = (profile.config.state_root.clone(), workspace.clone());
            if let Some(registered) = boundaries.get(&boundary) {
                if registered != &roots {
                    return Err(EmbeddedRuntimeError::Configuration(
                        "one Workspace identity must use one stable pair of persistent roots"
                            .into(),
                    ));
                }
            } else {
                for (owners, root, label) in [
                    (&state_root_owners, &roots.0, "state root"),
                    (&workspace_root_owners, &roots.1, "workspace root"),
                ] {
                    if owners
                        .get(root)
                        .is_some_and(|registered| registered != &boundary)
                    {
                        return Err(EmbeddedRuntimeError::Configuration(format!(
                            "{label} is owned by another Workspace identity"
                        )));
                    }
                }
                state_root_owners.insert(roots.0.clone(), boundary);
                workspace_root_owners.insert(roots.1.clone(), boundary);
                boundaries.insert(boundary, roots);
            }
            if by_identity
                .insert(profile.invocation, profile.config)
                .is_some()
            {
                return Err(EmbeddedRuntimeError::Configuration(
                    "duplicate invocation profile".into(),
                ));
            }
        }
        let mut retention_gates = HashMap::new();
        let mut retired_runs = HashMap::new();
        let mut tenant_retention_gates = HashMap::new();
        let mut state_root_leases = Vec::new();
        for config in by_identity.values() {
            if retention_gates.contains_key(&config.state_root) {
                continue;
            }
            state_root_leases.push(StateRootLease::acquire(&config.state_root)?);
            repair_committed_tombstones(&config.state_root, retention_policy)?;
            retired_runs.insert(
                config.state_root.clone(),
                Arc::new(Mutex::new(load_run_tombstone_index(&config.state_root)?)),
            );
            retention_gates.insert(config.state_root.clone(), Arc::new(Mutex::new(())));
        }
        for invocation in by_identity.keys() {
            tenant_retention_gates
                .entry(invocation.tenant_id)
                .or_insert_with(|| Arc::new(Mutex::new(())));
        }
        let runtime = Self {
            profiles: by_identity,
            admission,
            active: Arc::new(Mutex::new(HashMap::new())),
            recovery_gate: AsyncMutex::new(()),
            peak_active_execution_owners: AtomicUsize::new(0),
            event_subscriptions: Arc::new(EventSubscriptionCapacity::default()),
            retention_policy,
            retention_gates,
            retired_runs,
            tenant_retention_gates,
            _state_root_leases: state_root_leases,
        };
        let tenant_ids = runtime
            .tenant_retention_gates
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for tenant_id in tenant_ids {
            let gate = runtime.tenant_retention_gate(tenant_id)?;
            let _retention = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runtime.ensure_tenant_capacity_locked(tenant_id, 0)?;
            runtime.validate_tenant_tombstone_capacity(tenant_id)?;
        }
        Ok(runtime)
    }

    #[must_use]
    pub fn admission_snapshot(&self) -> RuntimeAdmissionSnapshot {
        self.admission.snapshot()
    }

    #[must_use]
    pub fn runtime_snapshot(&self) -> EmbeddedRuntimeSnapshot {
        let active_execution_owners = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let (
            active_event_subscriptions,
            buffered_event_slots,
            peak_active_event_subscriptions,
            peak_buffered_event_slots,
        ) = self.event_subscriptions.snapshot();
        EmbeddedRuntimeSnapshot {
            registered_profiles: self.profiles.len(),
            active_execution_owners,
            peak_active_execution_owners: self.peak_active_execution_owners.load(Ordering::Relaxed),
            active_event_subscriptions,
            buffered_event_slots,
            peak_active_event_subscriptions,
            peak_buffered_event_slots,
            admission: self.admission.snapshot(),
        }
    }

    pub async fn execute(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        self.execute_at_epoch(invocation, run_id, input, 1).await
    }

    pub async fn execute_at_epoch(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        if owner_epoch == 0 {
            return Err(EmbeddedRuntimeError::Configuration(
                "Workspace owner epoch must be positive".into(),
            ));
        }
        let (execution, active_guard) = self.claim_execution(invocation, run_id, false)?;
        let permit = self.admission.acquire(invocation).await?;
        let record = Self::new_run_record(invocation, run_id, input, owner_epoch);
        {
            let tenant_gate = self.tenant_retention_gate(invocation.tenant_id)?;
            let _tenant_retention = tenant_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let retention_gate = self.retention_gate(&config.state_root)?;
            {
                let _retention = retention_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.reject_retired_run(&config.state_root, invocation, run_id, input)?;
                if LocalRuntimeHost::read_run_record(&config.state_root, run_id)?.is_some() {
                    return Err(EmbeddedRuntimeError::Configuration(
                        "Run id already has durable state".into(),
                    ));
                }
            }
            self.ensure_tenant_capacity_locked(invocation.tenant_id, 1)?;
            let _retention = retention_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if count_run_directories(&config.state_root)?.saturating_add(1)
                > self.retention_policy.max_run_directories_per_workspace
            {
                let policy =
                    self.effective_retention_policy(invocation.tenant_id, &config.state_root)?;
                self.maintain_retention_locked(&config.state_root, 1, false, policy, 0)?;
            }
            // Maintenance may add tombstones for older Runs, never this absent
            // id. Recheck both authorities while the Workspace gate is held so
            // another invocation sharing the same root cannot reuse this id.
            self.reject_retired_run(&config.state_root, invocation, run_id, input)?;
            if LocalRuntimeHost::read_run_record(&config.state_root, run_id)?.is_some() {
                return Err(EmbeddedRuntimeError::Configuration(
                    "Run id already has durable state".into(),
                ));
            }
            Self::write_run_record(&config.state_root, &record)?;
        }
        self.drive_recorded(
            config,
            invocation,
            record,
            RecordedOperation::Execute,
            execution,
            active_guard,
            permit,
            None,
        )
        .await
        .map_err(Into::into)
    }

    pub async fn resume(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        let current = self.owned_run_record(invocation, run_id)?;
        if current.input != input {
            return Err(EmbeddedRuntimeError::Configuration(
                "resume input does not match the durable Run".into(),
            ));
        }
        if owner_epoch <= current.owner_epoch {
            return Err(EmbeddedRuntimeError::Configuration(
                "resume owner epoch must advance the durable Run".into(),
            ));
        }
        if !LocalRuntimeHost::checkpoint_path(&config.state_root, run_id).is_file() {
            return Err(EmbeddedRuntimeError::Configuration(
                "Run has no durable Checkpoint to resume".into(),
            ));
        }
        let (execution, active_guard) = self.claim_execution(invocation, run_id, false)?;
        let permit = self.admission.acquire(invocation).await?;
        let record = LocalRunRecord {
            owner_epoch,
            state: LocalRunState::Running,
            ..current
        };
        Self::write_run_record(&config.state_root, &record)?;
        self.drive_recorded(
            config,
            invocation,
            record,
            RecordedOperation::Resume,
            execution,
            active_guard,
            permit,
            None,
        )
        .await
        .map_err(Into::into)
    }

    /// Reads one bounded, versioned page from the durable event authority.
    /// Unlike the historical `replay_events` helper, this contract never
    /// materializes the complete log and distinguishes live, terminal,
    /// interrupted and retired history explicitly.
    pub fn event_cursor(
        &self,
        request: RuntimeEventCursorRequest,
    ) -> Result<RuntimeEventCursorPage, EmbeddedRuntimeError> {
        Self::validate_event_cursor_request(&request)?;
        let config = self.profile(request.invocation)?;
        Self::read_event_cursor_page(&config.state_root, &request).map_err(Into::into)
    }

    /// Compatibility shim for existing in-repository consumers that still
    /// require one complete hot event log (currently the paused Edge crate).
    /// New adapters must use `event_cursor` or `subscribe_events`; this helper
    /// cannot express retired history, typed boundaries or bounded pages.
    pub fn replay_events(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        after_sequence: u64,
    ) -> Result<Vec<LocalEvent>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        LocalRuntimeHost::replay_events(&config.state_root, run_id, after_sequence)
            .map_err(Into::into)
    }

    fn validate_event_cursor_request(
        request: &RuntimeEventCursorRequest,
    ) -> Result<(), RuntimeEventCursorError> {
        if request.schema_version != RUNTIME_EVENT_CURSOR_SCHEMA_VERSION {
            return Err(RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::UnsupportedSchema,
                "unsupported Runtime event cursor schema",
            ));
        }
        request.invocation.validate().map_err(|_| {
            RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::InvalidRequest,
                "Runtime event cursor invocation is invalid",
            )
        })?;
        if request.run_id.is_nil()
            || !(1..=RUNTIME_EVENT_CURSOR_MAX_EVENTS).contains(&request.limit)
        {
            return Err(RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::InvalidRequest,
                format!(
                    "Runtime event cursor requires a Run id and limit between 1 and {RUNTIME_EVENT_CURSOR_MAX_EVENTS}"
                ),
            ));
        }
        Ok(())
    }

    fn cursor_storage_error() -> RuntimeEventCursorError {
        RuntimeEventCursorError::new(
            RuntimeEventCursorErrorCode::StorageUnavailable,
            "Runtime event storage is unavailable",
        )
    }

    fn cursor_state_from_record(record: &LocalRunRecord) -> RuntimeEventCursorState {
        match &record.state {
            LocalRunState::Running
            | LocalRunState::ApprovalDecided { .. }
            | LocalRunState::McpInputDecided { .. } => RuntimeEventCursorState::Running,
            LocalRunState::Cancelling { .. } => RuntimeEventCursorState::Cancelling,
            LocalRunState::AwaitingApproval { .. } => RuntimeEventCursorState::WaitingApproval,
            LocalRunState::AwaitingMcpInput { .. } => RuntimeEventCursorState::Suspended,
            LocalRunState::Interrupted { .. } => RuntimeEventCursorState::Interrupted,
            LocalRunState::Finished { status } => RuntimeEventCursorState::Terminal {
                status: match status.as_str() {
                    "succeeded" => RunStatus::Succeeded,
                    "cancelled" => RunStatus::Cancelled,
                    "timed_out" => RunStatus::TimedOut,
                    "indeterminate" => RunStatus::Indeterminate,
                    _ => RunStatus::Failed,
                },
            },
            LocalRunState::Cancelled { .. } => RuntimeEventCursorState::Terminal {
                status: RunStatus::Cancelled,
            },
        }
    }

    fn terminal_event_status(event_type: &str) -> Option<RunStatus> {
        match event_type {
            "run.succeeded" => Some(RunStatus::Succeeded),
            "run.failed" => Some(RunStatus::Failed),
            "run.cancelled" => Some(RunStatus::Cancelled),
            "run.timed_out" => Some(RunStatus::TimedOut),
            "run.indeterminate" => Some(RunStatus::Indeterminate),
            _ => None,
        }
    }

    fn validate_cursor_event(
        event: &LocalEvent,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        expected_sequence: u64,
        terminal_already_seen: bool,
    ) -> Result<Option<RunStatus>, RuntimeEventCursorError> {
        let legacy_identity = [
            event.tenant_id,
            event.application_id,
            event.workload_identity_id,
            event.workspace_id,
            event.agent_version_id,
            event.model_policy_id,
        ]
        .iter()
        .all(Uuid::is_nil);
        let identity_matches = if legacy_identity {
            invocation == crate::local_invocation_context()
        } else {
            event.tenant_id == invocation.tenant_id
                && event.application_id == invocation.application_id
                && event.workload_identity_id == invocation.workload_identity_id
                && event.workspace_id == invocation.workspace_id
                && event.agent_version_id == invocation.agent_version_id
                && event.model_policy_id == invocation.model_policy_id
        };
        let digest_matches = (legacy_identity && event.digest.is_empty())
            || (!event.digest.is_empty()
                && event.digest
                    == hex::encode(Sha256::digest(serde_json::to_vec(&event.payload).map_err(
                        |_| {
                            RuntimeEventCursorError::new(
                                RuntimeEventCursorErrorCode::CorruptLog,
                                "durable event payload cannot be verified",
                            )
                        },
                    )?)));
        if event.run_id != run_id
            || !identity_matches
            || event.sequence != expected_sequence
            || !digest_matches
            || terminal_already_seen
        {
            return Err(RuntimeEventCursorError::new(
                if identity_matches {
                    RuntimeEventCursorErrorCode::CorruptLog
                } else {
                    RuntimeEventCursorErrorCode::IdentityMismatch
                },
                "durable event identity, digest or sequence is inconsistent",
            ));
        }
        Ok(Self::terminal_event_status(&event.event_type))
    }

    fn read_event_cursor_page(
        state_root: &Path,
        request: &RuntimeEventCursorRequest,
    ) -> Result<RuntimeEventCursorPage, RuntimeEventCursorError> {
        use std::io::{BufRead as _, Read as _};

        let record = LocalRuntimeHost::read_run_record(state_root, request.run_id)
            .map_err(|_| Self::cursor_storage_error())?;
        let tombstone = if record.is_none() {
            load_run_tombstone_index(state_root)
                .map_err(|_| Self::cursor_storage_error())?
                .remove(&request.run_id)
        } else {
            None
        };
        if let Some(tombstone) = tombstone {
            if tombstone.invocation != request.invocation {
                return Err(RuntimeEventCursorError::new(
                    RuntimeEventCursorErrorCode::IdentityMismatch,
                    "terminal event history is owned by another invocation",
                ));
            }
            if request.after_sequence > tombstone.terminal_sequence {
                return Err(RuntimeEventCursorError::new(
                    RuntimeEventCursorErrorCode::CursorAhead,
                    "event cursor is ahead of the retired terminal sequence",
                ));
            }
            return Ok(RuntimeEventCursorPage {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: request.invocation,
                run_id: request.run_id,
                requested_after_sequence: request.after_sequence,
                next_after_sequence: request.after_sequence,
                earliest_available_sequence: None,
                highest_committed_sequence: tombstone.terminal_sequence,
                history_gap: request.after_sequence < tombstone.terminal_sequence,
                has_more: false,
                state: RuntimeEventCursorState::Retired {
                    status: tombstone.status,
                    terminal_event_id: tombstone.terminal_event_id,
                    terminal_sequence: tombstone.terminal_sequence,
                    terminal_event_digest: tombstone.terminal_event_digest,
                },
                events: Vec::new(),
            });
        }
        let record = record.ok_or_else(|| {
            RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::NotFound,
                "Runtime event cursor Run was not found",
            )
        })?;
        if !Self::record_is_owned(request.invocation, &record) {
            return Err(RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::IdentityMismatch,
                "Runtime event history is owned by another invocation",
            ));
        }

        let event_path = state_root
            .join("runs")
            .join(request.run_id.to_string())
            .join("events.jsonl");
        let file = match std::fs::File::open(&event_path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(Self::cursor_storage_error()),
        };
        let mut events = Vec::with_capacity(request.limit.min(32));
        let mut highest = 0_u64;
        let mut terminal_status = None;
        if let Some(file) = file {
            let mut reader = std::io::BufReader::new(file);
            loop {
                let mut line = Vec::new();
                let mut bounded = reader
                    .by_ref()
                    .take((LOCAL_EVENT_LOG_LINE_MAX_BYTES + 1) as u64);
                let read = bounded
                    .read_until(b'\n', &mut line)
                    .map_err(|_| Self::cursor_storage_error())?;
                if read == 0 {
                    break;
                }
                if line.len() > LOCAL_EVENT_LOG_LINE_MAX_BYTES {
                    return Err(RuntimeEventCursorError::new(
                        RuntimeEventCursorErrorCode::CorruptLog,
                        "durable event log contains an oversized row",
                    ));
                }
                if line.last() != Some(&b'\n') {
                    // A JSONL row commits only with its final newline. A
                    // bounded prefix at EOF is an uncommitted crash tail;
                    // the next writer truncates it before appending.
                    break;
                }
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    return Err(RuntimeEventCursorError::new(
                        RuntimeEventCursorErrorCode::CorruptLog,
                        "durable event log contains an empty row",
                    ));
                }
                let event: LocalEvent = serde_json::from_slice(&line).map_err(|_| {
                    RuntimeEventCursorError::new(
                        RuntimeEventCursorErrorCode::CorruptLog,
                        "durable event log contains invalid JSON",
                    )
                })?;
                let expected_sequence = highest.saturating_add(1);
                let next_terminal = Self::validate_cursor_event(
                    &event,
                    request.invocation,
                    request.run_id,
                    expected_sequence,
                    terminal_status.is_some(),
                )?;
                highest = event.sequence;
                terminal_status = next_terminal;
                if event.sequence > request.after_sequence && events.len() < request.limit {
                    events.push(event);
                }
            }
        }

        if request.after_sequence > highest {
            return Err(RuntimeEventCursorError::new(
                RuntimeEventCursorErrorCode::CursorAhead,
                "event cursor is ahead of the highest committed sequence",
            ));
        }
        let record_state = Self::cursor_state_from_record(&record);
        let state = match (terminal_status, record_state) {
            (Some(event_status), RuntimeEventCursorState::Terminal { status })
                if event_status == status =>
            {
                RuntimeEventCursorState::Terminal {
                    status: event_status,
                }
            }
            // The terminal event is the commit point; a crash may leave the
            // record one transition behind until recovery reconciles it.
            (Some(event_status), RuntimeEventCursorState::Running)
            | (Some(event_status), RuntimeEventCursorState::Cancelling) => {
                RuntimeEventCursorState::Terminal {
                    status: event_status,
                }
            }
            (Some(_), _) | (None, RuntimeEventCursorState::Terminal { .. }) => {
                return Err(RuntimeEventCursorError::new(
                    RuntimeEventCursorErrorCode::CorruptLog,
                    "durable Run record and terminal event are inconsistent",
                ));
            }
            (None, state) => state,
        };
        let next_after_sequence = events
            .last()
            .map_or(request.after_sequence, |event| event.sequence);
        Ok(RuntimeEventCursorPage {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: request.invocation,
            run_id: request.run_id,
            requested_after_sequence: request.after_sequence,
            next_after_sequence,
            earliest_available_sequence: (highest > 0).then_some(1),
            highest_committed_sequence: highest,
            history_gap: false,
            has_more: next_after_sequence < highest,
            state,
            events,
        })
    }

    /// Follows one Run from the durable event log with a bounded in-memory
    /// channel. `after_sequence` has the same exclusive cursor semantics as
    /// `event_cursor`; reconnecting adapters can therefore resume without an
    /// in-memory broadcaster or a gap-prone best-effort queue.
    pub fn subscribe_events(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        after_sequence: u64,
        capacity: usize,
    ) -> Result<EmbeddedEventSubscription, EmbeddedRuntimeError> {
        if !(1..=EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY).contains(&capacity) {
            return Err(EmbeddedRuntimeError::Configuration(format!(
                "event subscription capacity must be between 1 and {EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY}"
            )));
        }
        let config = self.profile(invocation)?;
        let initial_page = Self::read_event_cursor_page(
            &config.state_root,
            &RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation,
                run_id,
                after_sequence,
                limit: capacity,
            },
        )?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            EmbeddedRuntimeError::Configuration(
                "event subscriptions require an active Tokio Runtime".into(),
            )
        })?;
        let permit = self.event_subscriptions.reserve(capacity)?;
        let state_root = config.state_root.clone();
        let (sender, receiver) = mpsc::channel(capacity);
        let stop = CancellationToken::new();
        let stop_for_task = stop.clone();
        let task = runtime.spawn(async move {
            use std::io::{BufRead as _, Read as _, Seek as _, SeekFrom};

            let mut cursor = after_sequence;
            if matches!(initial_page.state, RuntimeEventCursorState::Retired { .. }) {
                let boundary = RuntimeEventStreamItem::Boundary {
                    schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                    invocation,
                    run_id,
                    next_after_sequence: cursor,
                    earliest_available_sequence: initial_page.earliest_available_sequence,
                    highest_committed_sequence: initial_page.highest_committed_sequence,
                    history_gap: initial_page.history_gap,
                    state: initial_page.state,
                };
                let _ = sender.send(Ok(boundary)).await;
                return;
            }

            let event_path = state_root
                .join("runs")
                .join(run_id.to_string())
                .join("events.jsonl");
            let mut reader = None::<std::io::BufReader<std::fs::File>>;
            let mut committed_log_bytes = 0_u64;
            let mut last_log_sequence = 0_u64;
            let mut terminal_status = None;
            loop {
                if reader.is_none() {
                    match std::fs::File::open(&event_path) {
                        Ok(mut file) => {
                            if file.seek(SeekFrom::Start(committed_log_bytes)).is_err() {
                                let _ = sender.send(Err(Self::cursor_storage_error().into())).await;
                                return;
                            }
                            reader = Some(std::io::BufReader::new(file));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {
                            let _ = sender.send(Err(Self::cursor_storage_error().into())).await;
                            return;
                        }
                    }
                }

                let mut reopen_after_uncommitted_tail = false;
                let read = if let Some(reader) = reader.as_mut() {
                    let mut line = Vec::new();
                    let mut bounded = reader
                        .by_ref()
                        .take((LOCAL_EVENT_LOG_LINE_MAX_BYTES + 1) as u64);
                    match bounded.read_until(b'\n', &mut line) {
                        Ok(0) => None,
                        Ok(_) if line.len() > LOCAL_EVENT_LOG_LINE_MAX_BYTES => {
                            Some(Err(RuntimeEventCursorError::new(
                                RuntimeEventCursorErrorCode::CorruptLog,
                                "durable event log contains an oversized row",
                            )))
                        }
                        Ok(_) if line.last() != Some(&b'\n') => {
                            reopen_after_uncommitted_tail = true;
                            None
                        }
                        Ok(_) => Some(Ok(line)),
                        Err(_) => Some(Err(Self::cursor_storage_error())),
                    }
                } else {
                    None
                };
                if reopen_after_uncommitted_tail {
                    // A JSONL row commits only with its final newline. Treat
                    // this bounded EOF prefix as absent, then reopen after the
                    // writer has had a chance to truncate or finish it.
                    reader = None;
                }
                if let Some(read) = read {
                    let mut line = match read {
                        Ok(line) => line,
                        Err(error) => {
                            let _ = sender.send(Err(error.into())).await;
                            return;
                        }
                    };
                    let committed_row_bytes = line.len() as u64;
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let event: LocalEvent = match serde_json::from_slice(&line) {
                        Ok(event) => event,
                        Err(_) => {
                            let error = RuntimeEventCursorError::new(
                                RuntimeEventCursorErrorCode::CorruptLog,
                                "durable event log contains invalid JSON",
                            );
                            let _ = sender.send(Err(error.into())).await;
                            return;
                        }
                    };
                    let next_terminal = match Self::validate_cursor_event(
                        &event,
                        invocation,
                        run_id,
                        last_log_sequence.saturating_add(1),
                        terminal_status.is_some(),
                    ) {
                        Ok(status) => status,
                        Err(error) => {
                            let _ = sender.send(Err(error.into())).await;
                            return;
                        }
                    };
                    last_log_sequence = event.sequence;
                    committed_log_bytes = committed_log_bytes.saturating_add(committed_row_bytes);
                    terminal_status = next_terminal;
                    if event.sequence <= after_sequence {
                        continue;
                    }
                    cursor = event.sequence;
                    let item = RuntimeEventStreamItem::Event {
                        schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                        event: Box::new(event),
                    };
                    tokio::select! {
                        () = stop_for_task.cancelled() => return,
                        result = sender.send(Ok(item)) => {
                            if result.is_err() {
                                return;
                            }
                        }
                    }
                    continue;
                }

                let (state, earliest_available_sequence, highest_committed_sequence, history_gap) =
                    match LocalRuntimeHost::read_run_record(&state_root, run_id) {
                        Ok(Some(record)) => {
                            if !Self::record_is_owned(invocation, &record) {
                                let error = RuntimeEventCursorError::new(
                                    RuntimeEventCursorErrorCode::IdentityMismatch,
                                    "Runtime event history is owned by another invocation",
                                );
                                let _ = sender.send(Err(error.into())).await;
                                return;
                            }
                            let record_state = Self::cursor_state_from_record(&record);
                            let state = match (terminal_status, record_state) {
                                (
                                    Some(event_status),
                                    RuntimeEventCursorState::Terminal { status },
                                ) if event_status == status => RuntimeEventCursorState::Terminal {
                                    status: event_status,
                                },
                                (Some(event_status), RuntimeEventCursorState::Running)
                                | (Some(event_status), RuntimeEventCursorState::Cancelling) => {
                                    RuntimeEventCursorState::Terminal {
                                        status: event_status,
                                    }
                                }
                                (Some(_), _) | (None, RuntimeEventCursorState::Terminal { .. }) => {
                                    let error = RuntimeEventCursorError::new(
                                        RuntimeEventCursorErrorCode::CorruptLog,
                                        "durable Run record and terminal event are inconsistent",
                                    );
                                    let _ = sender.send(Err(error.into())).await;
                                    return;
                                }
                                (None, state) => state,
                            };
                            (
                                state,
                                (last_log_sequence > 0).then_some(1),
                                last_log_sequence,
                                false,
                            )
                        }
                        Ok(None) => {
                            let tombstone = match load_run_tombstone_index(&state_root) {
                                Ok(mut tombstones) => tombstones.remove(&run_id),
                                Err(_) => {
                                    let _ =
                                        sender.send(Err(Self::cursor_storage_error().into())).await;
                                    return;
                                }
                            };
                            let Some(tombstone) = tombstone else {
                                let error = RuntimeEventCursorError::new(
                                    RuntimeEventCursorErrorCode::NotFound,
                                    "Runtime event cursor Run was not found",
                                );
                                let _ = sender.send(Err(error.into())).await;
                                return;
                            };
                            if tombstone.invocation != invocation {
                                let error = RuntimeEventCursorError::new(
                                    RuntimeEventCursorErrorCode::IdentityMismatch,
                                    "terminal event history is owned by another invocation",
                                );
                                let _ = sender.send(Err(error.into())).await;
                                return;
                            }
                            (
                                RuntimeEventCursorState::Retired {
                                    status: tombstone.status,
                                    terminal_event_id: tombstone.terminal_event_id,
                                    terminal_sequence: tombstone.terminal_sequence,
                                    terminal_event_digest: tombstone.terminal_event_digest,
                                },
                                (last_log_sequence > 0).then_some(1),
                                tombstone.terminal_sequence,
                                cursor < tombstone.terminal_sequence
                                    && last_log_sequence < tombstone.terminal_sequence,
                            )
                        }
                        Err(_) => {
                            let _ = sender.send(Err(Self::cursor_storage_error().into())).await;
                            return;
                        }
                    };
                if matches!(
                    &state,
                    RuntimeEventCursorState::Running | RuntimeEventCursorState::Cancelling
                ) {
                    tokio::select! {
                        () = stop_for_task.cancelled() => return,
                        () = tokio::time::sleep(EMBEDDED_EVENT_LOG_POLL_INTERVAL) => {}
                    }
                    continue;
                }
                let boundary = RuntimeEventStreamItem::Boundary {
                    schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                    invocation,
                    run_id,
                    next_after_sequence: cursor,
                    earliest_available_sequence,
                    highest_committed_sequence,
                    history_gap,
                    state,
                };
                let _ = sender.send(Ok(boundary)).await;
                return;
            }
        });
        Ok(EmbeddedEventSubscription {
            receiver,
            stop,
            task,
            _permit: permit,
        })
    }

    pub fn read_run_record(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<Option<LocalRunRecord>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        let record = LocalRuntimeHost::read_run_record(&config.state_root, run_id)?;
        match record {
            Some(record) if Self::record_is_owned(invocation, &record) => Ok(Some(record)),
            Some(_) => Err(EmbeddedRuntimeError::Configuration(
                "Run record is owned by another invocation".into(),
            )),
            None => Ok(None),
        }
    }

    pub fn read_terminal_tombstone(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<Option<RuntimeTerminalTombstone>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        let tombstone = self.cached_terminal_tombstone(&config.state_root, run_id)?;
        match tombstone {
            Some(tombstone) if tombstone.invocation == invocation => Ok(Some(tombstone)),
            Some(_) => Err(EmbeddedRuntimeError::Configuration(
                "terminal tombstone is owned by another invocation".into(),
            )),
            None => Ok(None),
        }
    }

    /// Runs one synchronous, crash-safe maintenance pass for the canonical
    /// Workspace state root selected by `invocation`.
    pub fn maintain_retention(
        &self,
        invocation: RuntimeInvocationContext,
    ) -> Result<RuntimeRetentionReport, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        let tenant_gate = self.tenant_retention_gate(invocation.tenant_id)?;
        let _tenant_retention = tenant_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = self.retention_gate(&config.state_root)?;
        let _retention = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let policy = self.effective_retention_policy(invocation.tenant_id, &config.state_root)?;
        self.maintain_retention_locked(&config.state_root, 0, true, policy, 0)
    }

    pub fn read_control_receipt(
        &self,
        invocation: RuntimeInvocationContext,
        command_id: Uuid,
    ) -> Result<Option<RuntimeControlReceipt>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        let receipt = Self::load_control_receipt(&config.state_root, command_id)?;
        match receipt {
            Some(receipt) if receipt.invocation == invocation => {
                self.validate_control_command(&receipt.command())?;
                Ok(Some(receipt))
            }
            Some(_) => Err(EmbeddedRuntimeError::Configuration(
                "Runtime control receipt is owned by another invocation".into(),
            )),
            None => Ok(None),
        }
    }

    pub fn list_control_receipts(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<Vec<RuntimeControlReceipt>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?;
        let directory = config.state_root.join("control-receipts");
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string()).into()),
        };
        let mut receipts = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if !entry
                .file_type()
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
                .is_file()
            {
                continue;
            }
            let Some(command_id) = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                return Err(LocalRuntimeError::StateRoot(
                    "Runtime control receipt filename is invalid".into(),
                )
                .into());
            };
            let receipt = Self::load_control_receipt(&config.state_root, command_id)?
                .ok_or_else(|| LocalRuntimeError::StateRoot("control receipt vanished".into()))?;
            self.validate_control_command(&receipt.command())?;
            if receipt.invocation == invocation && receipt.run_id == run_id {
                receipts.push(receipt);
            }
        }
        receipts.sort_by_key(|receipt| receipt.command_id);
        Ok(receipts)
    }

    /// Starts one execution after its Run record is durably visible, while the
    /// returned task continues independently of the transport connection.
    pub async fn execute_detached(
        self: &Arc<Self>,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: String,
    ) -> Result<LocalRunRecord, EmbeddedRuntimeError> {
        self.profile(invocation)?;
        let runtime = Arc::clone(self);
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = runtime.execute(invocation, run_id, &input).await;
            let _ = finished_tx.send(result);
        });
        loop {
            if let Some(record) = self.read_run_record(invocation, run_id)?
                && (!matches!(&record.state, LocalRunState::Running)
                    || !self
                        .event_cursor(RuntimeEventCursorRequest {
                            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                            invocation,
                            run_id,
                            after_sequence: 0,
                            limit: 1,
                        })?
                        .events
                        .is_empty())
            {
                return Ok(record);
            }
            tokio::select! {
                result = &mut finished_rx => {
                    if let Some(record) = self.read_run_record(invocation, run_id)? {
                        return Ok(record);
                    }
                    return match result {
                        Ok(Ok(_)) => self.read_run_record(invocation, run_id)?.ok_or_else(|| {
                            EmbeddedRuntimeError::Configuration(
                                "completed execution lost its durable Run record".into(),
                            )
                        }),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(EmbeddedRuntimeError::Configuration(
                            "detached execution stopped before durable acceptance".into(),
                        )),
                    };
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
            }
        }
    }

    /// Durably accepts a control command, then lets its execution continue
    /// independently of the client connection that delivered it.
    pub async fn control_detached(
        self: &Arc<Self>,
        command: RuntimeControlCommand,
    ) -> Result<RuntimeControlReceipt, EmbeddedRuntimeError> {
        self.validate_control_command(&command)?;
        let config = self.profile(command.invocation)?;
        // The binding `control` enforces, applied *before* the caller is told
        // Accepted rather than after.
        //
        // Detached acceptance is a promise: the caller stops retrying and
        // records that its command landed. Letting a command id that is already
        // bound to a different action through that promise, only for the
        // asynchronous half to reject it, leaves the caller believing in a
        // receipt the ledger does not have. The local adapter never showed this
        // because it runs its own check first; a network caller has no such
        // adapter, which is how it surfaced.
        let digest = Self::control_command_digest(&command)?;
        let existing = Self::load_control_receipt(&config.state_root, command.command_id)?;
        if let Some(receipt) = existing.as_ref()
            && (receipt.command_digest != digest
                || receipt.invocation != command.invocation
                || receipt.run_id != command.run_id)
        {
            return Err(EmbeddedRuntimeError::ControlCommandRebound);
        }
        if let Some(receipt) = existing {
            if receipt.state == RuntimeControlReceiptState::Completed {
                return Ok(receipt);
            }
            if let Some(terminal) =
                Self::terminal_state_from_events(&config.state_root, command.run_id)?
            {
                let status = Self::terminal_status(&terminal).ok_or_else(|| {
                    EmbeddedRuntimeError::Configuration(
                        "terminal event did not map to a terminal Run status".into(),
                    )
                })?;
                let mut record = self.owned_run_record(command.invocation, command.run_id)?;
                record.state = terminal;
                Self::write_run_record(&config.state_root, &record)?;
                Self::complete_receipts_for_run(
                    &config.state_root,
                    command.invocation,
                    command.run_id,
                    status,
                )?;
                return Self::load_control_receipt(&config.state_root, command.command_id)?
                    .ok_or_else(|| {
                        EmbeddedRuntimeError::Configuration(
                            "terminal control receipt vanished during reconciliation".into(),
                        )
                    });
            }
        }
        let invocation = command.invocation;
        let command_id = command.command_id;
        let runtime = Arc::clone(self);
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = runtime.control(command).await;
            let _ = finished_tx.send(result);
        });
        loop {
            if let Some(receipt) = self.read_control_receipt(invocation, command_id)? {
                return Ok(receipt);
            }
            tokio::select! {
                result = &mut finished_rx => {
                    if let Some(receipt) = self.read_control_receipt(invocation, command_id)? {
                        return Ok(receipt);
                    }
                    return match result {
                        Ok(Ok(result)) => Ok(result.receipt),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(EmbeddedRuntimeError::Configuration(
                            "detached control stopped before durable acceptance".into(),
                        )),
                    };
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
            }
        }
    }

    fn plan_unfinished_recovery(
        &self,
        invocation: RuntimeInvocationContext,
    ) -> Result<VecDeque<PlannedRunRecovery>, EmbeddedRuntimeError> {
        let config = self.profile(invocation)?.clone();
        let records = LocalRuntimeHost::list_run_records(&config.state_root)?;
        let parent_owned_children =
            LocalRuntimeHost::managed_subagent_run_references(&config.state_root)?;
        let mut planned = VecDeque::new();
        for mut record in records {
            if !Self::record_is_owned(invocation, &record) {
                continue;
            }
            // Recovery only owns orphaned work. A Run still registered in
            // this process is either executing or projecting its terminal
            // event into the durable record and control receipts. Competing
            // with that finalizer can redispatch a model/Tool side effect or
            // observe a transient staging file.
            if self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&(invocation, record.run_id))
            {
                continue;
            }
            // The parent Checkpoint is the recovery authority for its child
            // graph. Resuming a child independently while its parent is also
            // being restored races two owner epochs and can execute the same
            // child continuation twice.
            if parent_owned_children.contains(&record.run_id) {
                continue;
            }
            if let Some(terminal) =
                Self::terminal_state_from_events(&config.state_root, record.run_id)?
            {
                record.state = terminal;
                Self::write_run_record(&config.state_root, &record)?;
                let status = Self::terminal_status(&record.state).ok_or_else(|| {
                    EmbeddedRuntimeError::Configuration(
                        "terminal event did not map to a terminal Run status".into(),
                    )
                })?;
                Self::complete_receipts_for_run(
                    &config.state_root,
                    invocation,
                    record.run_id,
                    status,
                )?;
                continue;
            }
            if Self::terminal_status(&record.state).is_some()
                || matches!(
                    &record.state,
                    LocalRunState::AwaitingApproval { .. }
                        | LocalRunState::AwaitingMcpInput { .. }
                        | LocalRunState::Interrupted { .. }
                )
            {
                continue;
            }

            let mut receipts = self.list_control_receipts(invocation, record.run_id)?;
            receipts.retain(|receipt| receipt.state == RuntimeControlReceiptState::Accepted);
            receipts.sort_by_key(|receipt| {
                if matches!(&receipt.action, RuntimeControlAction::Cancel { .. }) {
                    0u8
                } else {
                    1u8
                }
            });
            if !LocalRuntimeHost::checkpoint_path(&config.state_root, record.run_id).is_file() {
                let cancellation_reason =
                    receipts.iter().find_map(|receipt| match &receipt.action {
                        RuntimeControlAction::Cancel { reason } => Some(reason.clone()),
                        _ => None,
                    });
                let (state, status) = match (&record.state, cancellation_reason) {
                    (LocalRunState::Cancelling { reason }, _) => (
                        LocalRunState::Cancelled {
                            reason: reason.clone(),
                        },
                        RunStatus::Cancelled,
                    ),
                    (_, Some(reason)) => {
                        (LocalRunState::Cancelled { reason }, RunStatus::Cancelled)
                    }
                    _ => (
                        LocalRunState::Interrupted {
                            reason: "Runtime stopped before the Run produced a Checkpoint".into(),
                        },
                        RunStatus::Failed,
                    ),
                };
                record.state = state;
                Self::write_run_record(&config.state_root, &record)?;
                Self::complete_receipts_for_run(
                    &config.state_root,
                    invocation,
                    record.run_id,
                    status,
                )?;
                continue;
            }

            if !receipts.is_empty() {
                planned.push_back(PlannedRunRecovery {
                    commands: receipts
                        .into_iter()
                        .map(|receipt| receipt.command())
                        .collect(),
                });
                continue;
            }

            let action = match &record.state {
                LocalRunState::Running => RuntimeControlAction::Resume,
                LocalRunState::Cancelling { reason } => RuntimeControlAction::Cancel {
                    reason: reason.clone(),
                },
                LocalRunState::ApprovalDecided {
                    target_run_id,
                    approval_id,
                    binding_digest,
                    decision,
                } => RuntimeControlAction::DecideApproval {
                    target_run_id: *target_run_id,
                    approval_id: *approval_id,
                    binding_digest: binding_digest.clone(),
                    decision: *decision,
                },
                LocalRunState::McpInputDecided { resolution } => {
                    RuntimeControlAction::ResolveMcpInput {
                        input_id: resolution.input_id,
                        input_version: resolution.input_version,
                        binding_digest: resolution.binding_digest.clone(),
                        responses: resolution.responses.clone(),
                    }
                }
                _ => continue,
            };
            planned.push_back(PlannedRunRecovery {
                commands: vec![RuntimeControlCommand {
                    schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
                    command_id: Uuid::now_v7(),
                    invocation,
                    run_id: record.run_id,
                    expected_owner_epoch: record.owner_epoch,
                    action,
                }],
            });
        }
        Ok(planned)
    }

    async fn dispatch_planned_recovery(
        self: &Arc<Self>,
        plan: PlannedRunRecovery,
    ) -> Result<(), EmbeddedRuntimeError> {
        for command in plan.commands {
            self.control_detached(command).await?;
        }
        Ok(())
    }

    /// Reconciles every unfinished Run for one invocation and redispatches its
    /// durable control command. The daemon is only a transport adapter; owner
    /// epochs, exact decisions and cancellation precedence stay here.
    pub async fn recover_unfinished_detached(
        self: &Arc<Self>,
        invocation: RuntimeInvocationContext,
    ) -> Result<usize, EmbeddedRuntimeError> {
        let _recovery = self.recovery_gate.lock().await;
        let planned = self.plan_unfinished_recovery(invocation)?;
        let mut recovered = 0usize;
        for plan in planned {
            self.dispatch_planned_recovery(plan).await?;
            recovered += 1;
        }
        Ok(recovered)
    }

    /// Scans every registered invocation without making an embedding adapter
    /// enumerate tenant profiles or reproduce recovery ordering.
    ///
    /// Planning failures are isolated per profile. Runnable plans are then
    /// dispatched one Run per profile in round-robin order, so one tenant with
    /// many orphaned Runs cannot fill the admission queue before another
    /// tenant gets a recovery opportunity.
    pub async fn recover_all_unfinished_detached(
        self: &Arc<Self>,
    ) -> EmbeddedRuntimeRecoveryReport {
        let _recovery = self.recovery_gate.lock().await;
        let mut invocations = self.profiles.keys().copied().collect::<Vec<_>>();
        invocations.sort_by_key(|invocation| {
            (
                invocation.tenant_id,
                invocation.application_id,
                invocation.workspace_id,
                invocation.agent_version_id,
                invocation.model_policy_id,
                invocation.workload_identity_id,
            )
        });
        let scanned_profiles = invocations.len();
        let mut failures = Vec::new();
        let mut profile_plans = VecDeque::new();
        for invocation in invocations {
            match self.plan_unfinished_recovery(invocation) {
                Ok(plans) if !plans.is_empty() => {
                    profile_plans.push_back((invocation, plans));
                }
                Ok(_) => {}
                Err(error) => {
                    failures.push(EmbeddedRuntimeProfileRecoveryFailure { invocation, error })
                }
            }
        }

        let mut recovered_runs = 0usize;
        while let Some((invocation, mut plans)) = profile_plans.pop_front() {
            let plan = plans
                .pop_front()
                .expect("only non-empty recovery queues are scheduled");
            match self.dispatch_planned_recovery(plan).await {
                Ok(()) => {
                    recovered_runs += 1;
                    if !plans.is_empty() {
                        profile_plans.push_back((invocation, plans));
                    }
                }
                Err(error) => {
                    failures.push(EmbeddedRuntimeProfileRecoveryFailure { invocation, error })
                }
            }
        }
        failures.sort_by_key(|failure| {
            (
                failure.invocation.tenant_id,
                failure.invocation.application_id,
                failure.invocation.workspace_id,
                failure.invocation.agent_version_id,
                failure.invocation.model_policy_id,
                failure.invocation.workload_identity_id,
            )
        });
        EmbeddedRuntimeRecoveryReport {
            scanned_profiles,
            recovered_runs,
            failures,
        }
    }

    /// Applies a versioned approval, cancellation or crash-resume command. The
    /// implementation is deliberately below every transport adapter so Java,
    /// CLI and a future GUI cannot invent different recovery semantics.
    pub async fn control(
        &self,
        command: RuntimeControlCommand,
    ) -> Result<RuntimeControlResult, EmbeddedRuntimeError> {
        self.validate_control_command(&command)?;
        let config = self.profile(command.invocation)?.clone();
        let digest = Self::control_command_digest(&command)?;
        if let Some(retired) = read_retired_control(&config.state_root, command.command_id)? {
            if retired.command_digest != digest || retired.run_id != command.run_id {
                return Err(EmbeddedRuntimeError::Configuration(
                    "control command id was already retired for another command".into(),
                ));
            }
            let tombstone = self
                .cached_terminal_tombstone(&config.state_root, command.run_id)?
                .ok_or_else(|| {
                    EmbeddedRuntimeError::Configuration(
                        "retired control command lost its Run tombstone".into(),
                    )
                })?;
            if tombstone.invocation != command.invocation {
                return Err(EmbeddedRuntimeError::Configuration(
                    "retired control command belongs to another invocation".into(),
                ));
            }
            return Ok(RuntimeControlResult {
                receipt: RuntimeControlReceipt {
                    schema_version: RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
                    command_id: command.command_id,
                    command_digest: digest,
                    invocation: command.invocation,
                    run_id: command.run_id,
                    expected_owner_epoch: command.expected_owner_epoch,
                    action: command.action,
                    state: RuntimeControlReceiptState::Completed,
                    applied_owner_epoch: retired.applied_owner_epoch,
                    run_status: Some(retired.run_status),
                },
                outcome: None,
            });
        }
        if self
            .cached_terminal_tombstone(&config.state_root, command.run_id)?
            .is_some()
        {
            return Err(EmbeddedRuntimeError::Configuration(
                "terminal Run was retired and cannot accept a new control command".into(),
            ));
        }
        let existing = Self::load_control_receipt(&config.state_root, command.command_id)?;
        if let Some(receipt) = existing.as_ref() {
            if receipt.command_digest != digest
                || receipt.invocation != command.invocation
                || receipt.run_id != command.run_id
            {
                return Err(EmbeddedRuntimeError::ControlCommandRebound);
            }
            if receipt.state == RuntimeControlReceiptState::Completed {
                return Ok(RuntimeControlResult {
                    receipt: receipt.clone(),
                    outcome: None,
                });
            }
        }

        match command.action.clone() {
            RuntimeControlAction::Cancel { reason } => {
                self.apply_cancellation(command, digest, existing, reason)
                    .await
            }
            RuntimeControlAction::Resume => {
                self.apply_resume(command, digest, existing, None).await
            }
            RuntimeControlAction::DecideApproval {
                target_run_id,
                approval_id,
                binding_digest,
                decision,
            } => {
                let resolution = LocalApprovalResolution {
                    target_run_id,
                    approval_id: Some(approval_id),
                    binding_digest: Some(binding_digest),
                    decision,
                };
                self.apply_resume(
                    command,
                    digest,
                    existing,
                    Some(LocalResumeResolution::Approval(resolution)),
                )
                .await
            }
            RuntimeControlAction::ResolveMcpInput {
                input_id,
                input_version,
                binding_digest,
                responses,
            } => {
                let resolution = LocalMcpInputResolution {
                    input_id,
                    input_version,
                    binding_digest,
                    responses,
                };
                self.apply_resume(
                    command,
                    digest,
                    existing,
                    Some(LocalResumeResolution::McpInput(resolution)),
                )
                .await
            }
        }
    }

    async fn apply_resume(
        &self,
        command: RuntimeControlCommand,
        digest: String,
        existing: Option<RuntimeControlReceipt>,
        resolution: Option<LocalResumeResolution>,
    ) -> Result<RuntimeControlResult, EmbeddedRuntimeError> {
        let config = self.profile(command.invocation)?.clone();
        let current = self.owned_run_record(command.invocation, command.run_id)?;
        if let Some(status) = Self::terminal_status(&current.state) {
            if let Some(mut receipt) = existing {
                receipt.state = RuntimeControlReceiptState::Completed;
                receipt.run_status = Some(status);
                Self::write_control_receipt(&config.state_root, &receipt)?;
                return Ok(RuntimeControlResult {
                    receipt,
                    outcome: None,
                });
            }
            return Err(EmbeddedRuntimeError::Configuration(
                "terminal Run cannot accept a new resume command".into(),
            ));
        }
        if existing.is_none() && current.owner_epoch != command.expected_owner_epoch {
            return Err(EmbeddedRuntimeError::Configuration(
                "control command targets a stale owner epoch".into(),
            ));
        }
        let key = (command.invocation, command.run_id);
        if self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
        {
            if let Some(receipt) = existing {
                return Ok(RuntimeControlResult {
                    receipt,
                    outcome: None,
                });
            }
            return Err(EmbeddedRuntimeError::Configuration(
                "Run already has an active execution owner".into(),
            ));
        }
        let checkpoint = LocalRuntimeHost::checkpoint_path(&config.state_root, command.run_id);
        if !checkpoint.is_file() {
            return Err(EmbeddedRuntimeError::Configuration(
                "Run has no durable Checkpoint to resume".into(),
            ));
        }

        let next_epoch = current.owner_epoch.checked_add(1).ok_or_else(|| {
            EmbeddedRuntimeError::Configuration("Workspace owner epoch is exhausted".into())
        })?;
        let (state, operation) = match resolution {
            Some(LocalResumeResolution::Approval(resolution)) => {
                let approval_id = resolution
                    .approval_id
                    .expect("control approval always carries its id");
                let binding_digest = resolution
                    .binding_digest
                    .clone()
                    .expect("control approval always carries its binding");
                match &current.state {
                    LocalRunState::AwaitingApproval {
                        approval_id: expected_id,
                        binding_digest: expected_digest,
                        target_run_id,
                    } if *expected_id == approval_id
                        && expected_digest == &binding_digest
                        && target_run_id.unwrap_or(command.run_id) == resolution.target_run_id => {}
                    LocalRunState::ApprovalDecided {
                        target_run_id,
                        approval_id: expected_id,
                        binding_digest: expected_digest,
                        decision,
                    } if *target_run_id == resolution.target_run_id
                        && *expected_id == approval_id
                        && expected_digest == &binding_digest
                        && *decision == resolution.decision => {}
                    _ => {
                        return Err(EmbeddedRuntimeError::Configuration(
                            "approval command does not match the durable pending Tool".into(),
                        ));
                    }
                }
                LocalRuntimeHost::validate_approval_resolution_checkpoint(
                    &config.state_root,
                    command.run_id,
                    &resolution,
                )?;
                (
                    LocalRunState::ApprovalDecided {
                        target_run_id: resolution.target_run_id,
                        approval_id,
                        binding_digest,
                        decision: resolution.decision,
                    },
                    RecordedOperation::Approval(resolution),
                )
            }
            Some(LocalResumeResolution::McpInput(resolution)) => {
                match &current.state {
                    LocalRunState::AwaitingMcpInput { input }
                        if input.input_id == resolution.input_id
                            && input.binding_digest == resolution.binding_digest => {}
                    LocalRunState::McpInputDecided {
                        resolution: expected,
                    } if expected == &resolution => {}
                    _ => {
                        return Err(EmbeddedRuntimeError::Configuration(
                            "MCP input command does not match the durable pending request".into(),
                        ));
                    }
                }
                LocalRuntimeHost::validate_mcp_resolution_checkpoint(
                    &config.state_root,
                    command.run_id,
                    &resolution,
                )?;
                (
                    LocalRunState::McpInputDecided {
                        resolution: resolution.clone(),
                    },
                    RecordedOperation::McpInput(resolution),
                )
            }
            None => {
                if !matches!(current.state, LocalRunState::Running) {
                    return Err(EmbeddedRuntimeError::Configuration(
                        "Run is not in a crash-resumable state".into(),
                    ));
                }
                (LocalRunState::Running, RecordedOperation::Resume)
            }
        };
        let receipt = RuntimeControlReceipt {
            schema_version: RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
            command_id: command.command_id,
            command_digest: digest,
            invocation: command.invocation,
            run_id: command.run_id,
            expected_owner_epoch: command.expected_owner_epoch,
            action: command.action.clone(),
            state: RuntimeControlReceiptState::Accepted,
            applied_owner_epoch: next_epoch,
            run_status: None,
        };
        let (execution, active_guard) =
            self.claim_execution(command.invocation, command.run_id, false)?;
        let permit = self.admission.acquire(command.invocation).await?;
        Self::write_control_receipt(&config.state_root, &receipt)?;
        let record = LocalRunRecord {
            owner_epoch: next_epoch,
            state,
            ..current
        };
        Self::write_run_record(&config.state_root, &record)?;
        let outcome = self
            .drive_recorded(
                config.clone(),
                command.invocation,
                record,
                operation,
                execution,
                active_guard,
                permit,
                Some(receipt),
            )
            .await?;
        let receipt = Self::load_control_receipt(&config.state_root, command.command_id)?
            .ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "completed control command lost its durable receipt".into(),
                )
            })?;
        Ok(RuntimeControlResult {
            receipt,
            outcome: Some(outcome),
        })
    }

    async fn apply_cancellation(
        &self,
        command: RuntimeControlCommand,
        digest: String,
        existing: Option<RuntimeControlReceipt>,
        reason: String,
    ) -> Result<RuntimeControlResult, EmbeddedRuntimeError> {
        let config = self.profile(command.invocation)?.clone();
        let mut current = self.owned_run_record(command.invocation, command.run_id)?;
        if let Some(status) = Self::terminal_status(&current.state) {
            if let Some(mut receipt) = existing {
                receipt.state = RuntimeControlReceiptState::Completed;
                receipt.run_status = Some(status);
                Self::write_control_receipt(&config.state_root, &receipt)?;
                return Ok(RuntimeControlResult {
                    receipt,
                    outcome: None,
                });
            }
            return Err(EmbeddedRuntimeError::Configuration(
                "terminal Run cannot accept a new cancellation".into(),
            ));
        }
        if existing.is_none() && current.owner_epoch != command.expected_owner_epoch {
            return Err(EmbeddedRuntimeError::Configuration(
                "control command targets a stale owner epoch".into(),
            ));
        }
        let mut receipt = existing.unwrap_or(RuntimeControlReceipt {
            schema_version: RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
            command_id: command.command_id,
            command_digest: digest,
            invocation: command.invocation,
            run_id: command.run_id,
            expected_owner_epoch: command.expected_owner_epoch,
            action: command.action.clone(),
            state: RuntimeControlReceiptState::Accepted,
            applied_owner_epoch: current.owner_epoch,
            run_status: None,
        });

        if let Some(result) =
            self.cancel_active_execution(&config, &command, receipt.clone(), &reason)?
        {
            return Ok(result);
        }

        let (execution, active_guard) = loop {
            if let Some(claim) = self.try_claim_execution(command.invocation, command.run_id, true)
            {
                break claim;
            }
            if let Some(result) =
                self.cancel_active_execution(&config, &command, receipt.clone(), &reason)?
            {
                return Ok(result);
            }
        };
        current = self.owned_run_record(command.invocation, command.run_id)?;
        if let Some(status) = Self::terminal_status(&current.state) {
            receipt.state = RuntimeControlReceiptState::Completed;
            receipt.run_status = Some(status);
            Self::write_control_receipt(&config.state_root, &receipt)?;
            return Ok(RuntimeControlResult {
                receipt,
                outcome: None,
            });
        }
        if !matches!(
            current.state,
            LocalRunState::Running
                | LocalRunState::Cancelling { .. }
                | LocalRunState::AwaitingApproval { .. }
                | LocalRunState::AwaitingMcpInput { .. }
        ) {
            return Err(EmbeddedRuntimeError::Configuration(
                "Run is not cancellable".into(),
            ));
        }

        let has_checkpoint =
            LocalRuntimeHost::checkpoint_path(&config.state_root, command.run_id).is_file();
        let next_epoch = current.owner_epoch.checked_add(1).ok_or_else(|| {
            EmbeddedRuntimeError::Configuration("Workspace owner epoch is exhausted".into())
        })?;
        let permit = self.admission.acquire(command.invocation).await?;
        receipt.applied_owner_epoch = next_epoch;
        Self::write_control_receipt(&config.state_root, &receipt)?;
        let record = LocalRunRecord {
            owner_epoch: next_epoch,
            state: LocalRunState::Cancelling { reason },
            ..current
        };
        Self::write_run_record(&config.state_root, &record)?;
        let outcome = self
            .drive_recorded(
                config.clone(),
                command.invocation,
                record,
                if has_checkpoint {
                    RecordedOperation::Resume
                } else {
                    RecordedOperation::Execute
                },
                execution,
                active_guard,
                permit,
                Some(receipt),
            )
            .await?;
        let receipt = Self::load_control_receipt(&config.state_root, command.command_id)?
            .ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "completed cancellation lost its durable receipt".into(),
                )
            })?;
        Ok(RuntimeControlResult {
            receipt,
            outcome: Some(outcome),
        })
    }

    fn cancel_active_execution(
        &self,
        config: &LocalRuntimeConfig,
        command: &RuntimeControlCommand,
        mut receipt: RuntimeControlReceipt,
        reason: &str,
    ) -> Result<Option<RuntimeControlResult>, EmbeddedRuntimeError> {
        let key = (command.invocation, command.run_id);
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        let Some(active) = active else {
            return Ok(None);
        };
        let _gate = active
            .record_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.owned_run_record(command.invocation, command.run_id)?;
        if let Some(status) = Self::terminal_status(&current.state) {
            receipt.state = RuntimeControlReceiptState::Completed;
            receipt.run_status = Some(status);
            Self::write_control_receipt(&config.state_root, &receipt)?;
            return Ok(Some(RuntimeControlResult {
                receipt,
                outcome: None,
            }));
        }
        if active.finalizing.load(Ordering::Acquire) {
            return Err(EmbeddedRuntimeError::Configuration(
                "Run execution has finished and its durable state is being committed".into(),
            ));
        }
        if !matches!(
            current.state,
            LocalRunState::Running | LocalRunState::Cancelling { .. }
        ) {
            return Err(EmbeddedRuntimeError::Configuration(
                "active Run is not cancellable".into(),
            ));
        }
        Self::write_control_receipt(&config.state_root, &receipt)?;
        let cancelling = LocalRunRecord {
            state: LocalRunState::Cancelling {
                reason: reason.to_owned(),
            },
            ..current
        };
        Self::write_run_record(&config.state_root, &cancelling)?;
        let mut commands = active
            .cancellation_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !commands.contains(&command.command_id) {
            commands.push(command.command_id);
        }
        drop(commands);
        active.cancellation.cancel();
        Ok(Some(RuntimeControlResult {
            receipt,
            outcome: None,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_recorded(
        &self,
        config: LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
        record: LocalRunRecord,
        operation: RecordedOperation,
        execution: Arc<ActiveExecution>,
        _active_guard: ActiveExecutionGuard,
        _permit: RuntimeAdmissionPermit,
        control_receipt: Option<RuntimeControlReceipt>,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let cancellation = execution.cancellation.clone();
        let mut host = LocalRuntimeHost::start_for_invocation_with_cancellation(
            config.clone(),
            invocation,
            cancellation.clone(),
        )?;
        let result = match operation {
            RecordedOperation::Execute => {
                host.execute_as_at_epoch(record.run_id, &record.input, record.owner_epoch)
                    .await
            }
            RecordedOperation::Resume => {
                host.resume(record.run_id, &record.input, record.owner_epoch)
                    .await
            }
            RecordedOperation::Approval(resolution) => {
                host.resume_with_resolution(
                    record.run_id,
                    &record.input,
                    record.owner_epoch,
                    resolution,
                )
                .await
            }
            RecordedOperation::McpInput(resolution) => {
                host.resume_with_mcp_input(
                    record.run_id,
                    &record.input,
                    record.owner_epoch,
                    resolution,
                )
                .await
            }
        };
        {
            let _gate = execution
                .record_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            execution.finalizing.store(true, Ordering::Release);
        }
        host.shutdown().await;

        let _gate = execution
            .record_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let committed_cancellation =
            LocalRuntimeHost::read_run_record(&config.state_root, record.run_id)?.and_then(
                |current| match current.state {
                    LocalRunState::Cancelling { reason } => Some(reason),
                    _ => None,
                },
            );
        let terminal_from_events =
            Self::terminal_state_from_events(&config.state_root, record.run_id)?;
        let (state, status) = if let Some(terminal) = terminal_from_events {
            let status = Self::terminal_status(&terminal).ok_or_else(|| {
                LocalRuntimeError::StateRoot(
                    "terminal Kernel event did not map to a terminal Run status".into(),
                )
            })?;
            let state = match (status, committed_cancellation) {
                (RunStatus::Cancelled, Some(reason)) => LocalRunState::Cancelled { reason },
                _ => terminal,
            };
            (state, status)
        } else {
            let outcome = match &result {
                Ok(outcome) => outcome,
                Err(_) => return result,
            };
            let (state, status) = Self::recorded_outcome_state(outcome);
            if status.is_terminal() {
                return Err(LocalRuntimeError::StateRoot(
                    "terminal Runtime outcome has no committed Kernel terminal event".into(),
                ));
            }
            (state, status)
        };
        let updated = LocalRunRecord { state, ..record };
        Self::write_run_record(&config.state_root, &updated)?;
        if let Some(mut receipt) = control_receipt {
            receipt.state = RuntimeControlReceiptState::Completed;
            receipt.run_status = Some(status);
            Self::write_control_receipt(&config.state_root, &receipt)?;
        }
        Self::complete_cancellation_receipts(&config.state_root, &execution, status)?;
        result
    }

    fn complete_cancellation_receipts(
        state_root: &Path,
        execution: &ActiveExecution,
        status: RunStatus,
    ) -> Result<(), LocalRuntimeError> {
        let cancellation_commands = execution
            .cancellation_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for command_id in cancellation_commands {
            if let Some(mut receipt) = Self::load_control_receipt(state_root, command_id)? {
                receipt.state = RuntimeControlReceiptState::Completed;
                receipt.run_status = Some(status);
                Self::write_control_receipt(state_root, &receipt)?;
            }
        }
        Ok(())
    }

    fn claim_execution(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        cancel_on_start: bool,
    ) -> Result<(Arc<ActiveExecution>, ActiveExecutionGuard), EmbeddedRuntimeError> {
        self.try_claim_execution(invocation, run_id, cancel_on_start)
            .ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "Run already has an active execution owner".into(),
                )
            })
    }

    fn try_claim_execution(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        cancel_on_start: bool,
    ) -> Option<(Arc<ActiveExecution>, ActiveExecutionGuard)> {
        let key = (invocation, run_id);
        let cancellation = CancellationToken::new();
        if cancel_on_start {
            cancellation.cancel();
        }
        let execution = Arc::new(ActiveExecution {
            cancellation,
            finalizing: AtomicBool::new(false),
            record_gate: Mutex::new(()),
            cancellation_commands: Mutex::new(Vec::new()),
        });
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(&key) {
            return None;
        }
        active.insert(key, Arc::clone(&execution));
        let active_execution_owners = active.len();
        drop(active);
        self.peak_active_execution_owners
            .fetch_max(active_execution_owners, Ordering::Relaxed);
        let guard = ActiveExecutionGuard {
            active: Arc::clone(&self.active),
            key,
            execution: Arc::clone(&execution),
        };
        Some((execution, guard))
    }

    fn new_run_record(
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> LocalRunRecord {
        LocalRunRecord {
            store_version: crate::LOCAL_STORE_VERSION,
            tenant_id: invocation.tenant_id,
            application_id: invocation.application_id,
            workload_identity_id: invocation.workload_identity_id,
            workspace_id: invocation.workspace_id,
            agent_version_id: invocation.agent_version_id,
            model_policy_id: invocation.model_policy_id,
            run_id,
            input: input.to_owned(),
            state: LocalRunState::Running,
            owner_epoch,
        }
    }

    fn recorded_outcome_state(outcome: &LocalRunOutcome) -> (LocalRunState, RunStatus) {
        if let Some(approval) = &outcome.pending_approval {
            return (
                LocalRunState::AwaitingApproval {
                    approval_id: approval.approval_id,
                    binding_digest: approval.binding_digest.clone(),
                    target_run_id: Some(approval.target_run_id),
                },
                RunStatus::WaitingApproval,
            );
        }
        if let Some(input) = &outcome.pending_mcp_input {
            return (
                LocalRunState::AwaitingMcpInput {
                    input: input.clone(),
                },
                RunStatus::Suspended,
            );
        }
        match outcome.status {
            RunStatus::Cancelled => (
                LocalRunState::Cancelled {
                    reason: "the Runtime execution was cancelled".into(),
                },
                RunStatus::Cancelled,
            ),
            status => (
                LocalRunState::Finished {
                    status: status.as_str().into(),
                },
                status,
            ),
        }
    }

    fn terminal_status(state: &LocalRunState) -> Option<RunStatus> {
        match state {
            LocalRunState::Finished { status } => match status.as_str() {
                "succeeded" => Some(RunStatus::Succeeded),
                "cancelled" => Some(RunStatus::Cancelled),
                "timed_out" => Some(RunStatus::TimedOut),
                "indeterminate" => Some(RunStatus::Indeterminate),
                _ => Some(RunStatus::Failed),
            },
            LocalRunState::Cancelled { .. } => Some(RunStatus::Cancelled),
            _ => None,
        }
    }

    fn terminal_state_from_events(
        state_root: &Path,
        run_id: Uuid,
    ) -> Result<Option<LocalRunState>, LocalRuntimeError> {
        let events = LocalRuntimeHost::replay_events(state_root, run_id, 0)?;
        Ok(events
            .iter()
            .rev()
            .find_map(|event| match event.event_type.as_str() {
                "run.succeeded" => Some(LocalRunState::Finished {
                    status: "succeeded".into(),
                }),
                "run.failed" => Some(LocalRunState::Finished {
                    status: "failed".into(),
                }),
                "run.cancelled" => Some(LocalRunState::Cancelled {
                    reason: "the Kernel attempt was cancelled".into(),
                }),
                "run.timed_out" => Some(LocalRunState::Finished {
                    status: "timed_out".into(),
                }),
                "run.indeterminate" => Some(LocalRunState::Finished {
                    status: "indeterminate".into(),
                }),
                _ => None,
            }))
    }

    fn complete_receipts_for_run(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        status: RunStatus,
    ) -> Result<(), LocalRuntimeError> {
        let directory = state_root.join("control-receipts");
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if !entry
                .file_type()
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
                .is_file()
            {
                continue;
            }
            let command_id = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    LocalRuntimeError::StateRoot(
                        "Runtime control receipt filename is invalid".into(),
                    )
                })?;
            let Some(mut receipt) = Self::load_control_receipt(state_root, command_id)? else {
                return Err(LocalRuntimeError::StateRoot(
                    "Runtime control receipt vanished".into(),
                ));
            };
            if receipt.invocation == invocation
                && receipt.run_id == run_id
                && receipt.state == RuntimeControlReceiptState::Accepted
            {
                receipt.state = RuntimeControlReceiptState::Completed;
                receipt.run_status = Some(status);
                Self::write_control_receipt(state_root, &receipt)?;
            }
        }
        Ok(())
    }

    fn record_is_owned(invocation: RuntimeInvocationContext, record: &LocalRunRecord) -> bool {
        record.tenant_id == invocation.tenant_id
            && record.application_id == invocation.application_id
            && record.workload_identity_id == invocation.workload_identity_id
            && record.workspace_id == invocation.workspace_id
            && record.agent_version_id == invocation.agent_version_id
            && record.model_policy_id == invocation.model_policy_id
    }

    fn owned_run_record(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<LocalRunRecord, EmbeddedRuntimeError> {
        self.read_run_record(invocation, run_id)?
            .ok_or_else(|| EmbeddedRuntimeError::Configuration("unknown Run".into()))
    }

    fn validate_control_command(
        &self,
        command: &RuntimeControlCommand,
    ) -> Result<(), EmbeddedRuntimeError> {
        if command.schema_version != RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION {
            return Err(EmbeddedRuntimeError::InvalidControlCommand(
                "unsupported Runtime control command schema".into(),
            ));
        }
        command.invocation.validate().map_err(|error| {
            EmbeddedRuntimeError::InvalidControlCommand(format!(
                "invalid Runtime invocation: {error}"
            ))
        })?;
        if command.command_id.is_nil()
            || command.run_id.is_nil()
            || command.expected_owner_epoch == 0
        {
            return Err(EmbeddedRuntimeError::InvalidControlCommand(
                "Runtime control command identity is incomplete".into(),
            ));
        }
        match &command.action {
            RuntimeControlAction::Resume => {}
            RuntimeControlAction::Cancel { reason } => {
                if reason.trim().is_empty() || reason.len() > 512 {
                    return Err(EmbeddedRuntimeError::InvalidControlCommand(
                        "cancellation reason must contain 1 to 512 bytes".into(),
                    ));
                }
            }
            RuntimeControlAction::DecideApproval {
                target_run_id,
                approval_id,
                binding_digest,
                ..
            } => {
                if target_run_id.is_nil()
                    || approval_id.is_nil()
                    || !Self::is_sha256(binding_digest)
                {
                    return Err(EmbeddedRuntimeError::InvalidControlCommand(
                        "approval command binding is invalid".into(),
                    ));
                }
            }
            RuntimeControlAction::ResolveMcpInput {
                input_id,
                input_version,
                binding_digest,
                responses,
            } => {
                if input_id.is_nil()
                    || *input_version != agent_protocol::MCP_INPUT_VERSION
                    || !Self::is_sha256(binding_digest)
                    || responses.is_empty()
                    || responses.len() > 64
                {
                    return Err(EmbeddedRuntimeError::InvalidControlCommand(
                        "MCP input command binding is invalid".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn is_sha256(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn retention_gate(&self, state_root: &Path) -> Result<Arc<Mutex<()>>, EmbeddedRuntimeError> {
        self.retention_gates
            .get(state_root)
            .cloned()
            .ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "canonical Workspace state root has no retention gate".into(),
                )
            })
    }

    fn tenant_retention_gate(
        &self,
        tenant_id: Uuid,
    ) -> Result<Arc<Mutex<()>>, EmbeddedRuntimeError> {
        self.tenant_retention_gates
            .get(&tenant_id)
            .cloned()
            .ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "registered tenant has no Runtime retention gate".into(),
                )
            })
    }

    fn tenant_state_roots(&self, tenant_id: Uuid) -> Vec<PathBuf> {
        let mut roots = self
            .profiles
            .iter()
            .filter(|(invocation, _)| invocation.tenant_id == tenant_id)
            .map(|(_, config)| config.state_root.clone())
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();
        roots
    }

    fn effective_retention_policy(
        &self,
        tenant_id: Uuid,
        state_root: &Path,
    ) -> Result<RuntimeRetentionPolicy, EmbeddedRuntimeError> {
        let mut other_run_tombstones = 0usize;
        let mut other_control_tombstones = 0usize;
        for root in self.tenant_state_roots(tenant_id) {
            if root == state_root {
                continue;
            }
            let (runs, controls, _) = ledger_counts_and_bytes(&root)?;
            other_run_tombstones = other_run_tombstones.saturating_add(runs);
            other_control_tombstones = other_control_tombstones.saturating_add(controls);
        }
        let mut policy = self.retention_policy;
        policy.max_run_tombstones_per_workspace = policy.max_run_tombstones_per_workspace.min(
            policy
                .max_run_tombstones_per_tenant
                .saturating_sub(other_run_tombstones),
        );
        policy.max_control_tombstones_per_workspace =
            policy.max_control_tombstones_per_workspace.min(
                policy
                    .max_control_tombstones_per_tenant
                    .saturating_sub(other_control_tombstones),
            );
        Ok(policy)
    }

    fn ensure_tenant_capacity_locked(
        &self,
        tenant_id: Uuid,
        reserve_run_directories: usize,
    ) -> Result<(), EmbeddedRuntimeError> {
        let mut roots = self
            .tenant_state_roots(tenant_id)
            .into_iter()
            .map(|root| {
                let directories = count_run_directories(&root)?;
                Ok((root, directories))
            })
            .collect::<Result<Vec<_>, LocalRuntimeError>>()?;
        let mut total_directories = roots
            .iter()
            .fold(0usize, |total, (_, count)| total.saturating_add(*count));
        let mut required_retirements = total_directories
            .saturating_add(reserve_run_directories)
            .saturating_sub(self.retention_policy.max_run_directories_per_tenant);
        if required_retirements == 0 {
            return Ok(());
        }
        roots.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for (root, _) in &roots {
            if required_retirements == 0 {
                break;
            }
            let gate = self.retention_gate(root)?;
            let _retention = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let policy = self.effective_retention_policy(tenant_id, root)?;
            let report =
                self.maintain_retention_locked(root, 0, false, policy, required_retirements)?;
            required_retirements = required_retirements.saturating_sub(report.tombstoned_runs);
        }
        total_directories = 0;
        for (root, _) in roots {
            total_directories = total_directories.saturating_add(count_run_directories(&root)?);
        }
        if total_directories.saturating_add(reserve_run_directories)
            > self.retention_policy.max_run_directories_per_tenant
        {
            return Err(EmbeddedRuntimeError::Configuration(
                "tenant Runtime state capacity is exhausted; no eligible terminal evidence can be retired safely"
                    .into(),
            ));
        }
        self.validate_tenant_tombstone_capacity(tenant_id)
    }

    fn validate_tenant_tombstone_capacity(
        &self,
        tenant_id: Uuid,
    ) -> Result<(), EmbeddedRuntimeError> {
        let mut total_run_tombstones = 0usize;
        let mut total_control_tombstones = 0usize;
        for root in self.tenant_state_roots(tenant_id) {
            let (runs, controls, _) = ledger_counts_and_bytes(&root)?;
            total_run_tombstones = total_run_tombstones.saturating_add(runs);
            total_control_tombstones = total_control_tombstones.saturating_add(controls);
        }
        if total_run_tombstones <= self.retention_policy.max_run_tombstones_per_tenant
            && total_control_tombstones <= self.retention_policy.max_control_tombstones_per_tenant
        {
            return Ok(());
        }
        Err(EmbeddedRuntimeError::Configuration(
            "tenant Runtime tombstone capacity is exhausted".into(),
        ))
    }

    fn reject_retired_run(
        &self,
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
    ) -> Result<(), EmbeddedRuntimeError> {
        let Some(tombstone) = self.cached_terminal_tombstone(state_root, run_id)? else {
            return Ok(());
        };
        let requested = run_binding_digest(invocation, run_id, input);
        let message =
            if tombstone.invocation == invocation && tombstone.run_binding_digest == requested {
                "retired Run replay was refused"
            } else {
                "Run id conflicts with retired durable evidence"
            };
        Err(EmbeddedRuntimeError::Configuration(message.into()))
    }

    fn maintain_retention_locked(
        &self,
        state_root: &Path,
        reserve_run_directories: usize,
        force_retention_target: bool,
        policy: RuntimeRetentionPolicy,
        minimum_retirements: usize,
    ) -> Result<RuntimeRetentionReport, EmbeddedRuntimeError> {
        let repaired_tombstones = repair_committed_tombstones(state_root, policy)?;
        let scan = scan_retention_candidates(state_root, policy, Utc::now())?;
        let desired_for_retention = scan
            .terminal_records
            .saturating_sub(policy.retain_terminal_runs_per_workspace);
        let desired_for_capacity = scan
            .run_directories
            .saturating_add(reserve_run_directories)
            .saturating_sub(policy.max_run_directories_per_workspace);
        let desired = if force_retention_target || desired_for_capacity > 0 {
            desired_for_retention
                .max(desired_for_capacity)
                .max(minimum_retirements)
        } else {
            minimum_retirements
        };
        let (mut run_capacity, mut control_capacity) = available_tombstone_capacity(&scan, policy);
        let mut selected = Vec::new();
        let mut maintenance_guards = Vec::new();
        for candidate in scan.candidates.iter().cloned() {
            if selected.len() >= desired || run_capacity == 0 {
                break;
            }
            let controls = candidate.control_tombstones.len();
            if controls > control_capacity || !self.profiles.contains_key(&candidate.invocation) {
                continue;
            }
            let Some((_execution, guard)) =
                self.try_claim_execution(candidate.invocation, candidate.run_id, false)
            else {
                continue;
            };
            run_capacity -= 1;
            control_capacity -= controls;
            selected.push(candidate);
            maintenance_guards.push(guard);
        }
        let (tombstoned_runs, tombstoned_control_commands) =
            commit_retention_candidates(state_root, policy, selected)?;
        drop(maintenance_guards);
        if tombstoned_runs > 0 || repaired_tombstones > 0 {
            let refreshed = load_run_tombstone_index(state_root)?;
            let cache = self.retired_runs.get(state_root).ok_or_else(|| {
                EmbeddedRuntimeError::Configuration(
                    "canonical Workspace state root has no tombstone index".into(),
                )
            })?;
            *cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = refreshed;
        }
        let run_directories_after = count_run_directories(state_root)?;
        if run_directories_after.saturating_add(reserve_run_directories)
            > policy.max_run_directories_per_workspace
        {
            return Err(EmbeddedRuntimeError::Configuration(
                "Workspace Runtime state capacity is exhausted; no eligible terminal evidence can be retired safely"
                    .into(),
            ));
        }
        let (total_run_tombstones, total_control_tombstones, terminal_ledger_bytes) =
            ledger_counts_and_bytes(state_root)?;
        Ok(RuntimeRetentionReport {
            run_directories_before: scan.run_directories,
            run_directories_after,
            terminal_records_before: scan.terminal_records,
            unmanaged_run_directories: scan.unmanaged_run_directories,
            strongly_referenced_runs: scan.strongly_referenced_runs,
            tombstoned_runs,
            tombstoned_control_commands,
            repaired_tombstones,
            total_run_tombstones,
            total_control_tombstones,
            terminal_ledger_bytes,
        })
    }

    fn cached_terminal_tombstone(
        &self,
        state_root: &Path,
        run_id: Uuid,
    ) -> Result<Option<RuntimeTerminalTombstone>, EmbeddedRuntimeError> {
        #[cfg(not(unix))]
        return Ok(load_run_tombstone_index(state_root)?.get(&run_id).cloned());

        #[cfg(unix)]
        let cache = self.retired_runs.get(state_root).ok_or_else(|| {
            EmbeddedRuntimeError::Configuration(
                "canonical Workspace state root has no tombstone index".into(),
            )
        })?;
        #[cfg(unix)]
        return Ok(cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .cloned());
    }

    fn control_command_digest(
        command: &RuntimeControlCommand,
    ) -> Result<String, EmbeddedRuntimeError> {
        let encoded = serde_json::to_vec(command)
            .map_err(|error| EmbeddedRuntimeError::Configuration(error.to_string()))?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }

    fn control_receipt_path(state_root: &Path, command_id: Uuid) -> PathBuf {
        state_root
            .join("control-receipts")
            .join(format!("{command_id}.json"))
    }

    fn load_control_receipt(
        state_root: &Path,
        command_id: Uuid,
    ) -> Result<Option<RuntimeControlReceipt>, LocalRuntimeError> {
        let path = Self::control_receipt_path(state_root, command_id);
        let body = match std::fs::read(path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let receipt: RuntimeControlReceipt = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let reconstructed_digest = Self::control_command_digest(&receipt.command())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if receipt.schema_version != RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION
            || receipt.command_id != command_id
            || receipt.expected_owner_epoch == 0
            || !Self::is_sha256(&receipt.command_digest)
            || receipt.command_digest != reconstructed_digest
        {
            return Err(LocalRuntimeError::StateRoot(
                "Runtime control receipt is invalid".into(),
            ));
        }
        Ok(Some(receipt))
    }

    fn write_control_receipt(
        state_root: &Path,
        receipt: &RuntimeControlReceipt,
    ) -> Result<(), LocalRuntimeError> {
        let path = Self::control_receipt_path(state_root, receipt.command_id);
        let body = serde_json::to_vec_pretty(receipt)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Self::durable_replace(&path, &body)
    }

    fn write_run_record(
        state_root: &Path,
        record: &LocalRunRecord,
    ) -> Result<(), LocalRuntimeError> {
        LocalRuntimeHost::write_run_record(state_root, record)
    }

    fn durable_replace(path: &Path, body: &[u8]) -> Result<(), LocalRuntimeError> {
        let parent = path
            .parent()
            .ok_or_else(|| LocalRuntimeError::StateRoot("state path has no parent".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let staging = path.with_extension("json.partial");
        use std::io::Write as _;
        let mut file = std::fs::File::create(&staging)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        file.write_all(body)
            .and_then(|()| file.sync_all())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        std::fs::rename(&staging, path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Ok(())
    }

    fn profile(
        &self,
        invocation: RuntimeInvocationContext,
    ) -> Result<&LocalRuntimeConfig, EmbeddedRuntimeError> {
        invocation
            .validate()
            .map_err(|error| EmbeddedRuntimeError::Configuration(error.to_string()))?;
        self.profiles
            .get(&invocation)
            .ok_or(EmbeddedRuntimeError::UnregisteredInvocation)
    }
}
