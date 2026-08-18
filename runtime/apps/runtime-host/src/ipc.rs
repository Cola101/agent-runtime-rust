//! Long-running local host and its Unix-socket IPC (ADR-0035 decision 7).
//!
//! The client is not the Runtime. A Run is owned by the daemon and by the local
//! store, so disconnecting a client must never cancel work, and reconnecting
//! must reconstruct what happened from the durable event log rather than from
//! anything the client remembered.

use crate::admission::RuntimeAdmissionLimits;
use crate::embedded::{
    EmbeddedRuntime, EmbeddedRuntimeError, RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
    RuntimeControlAction, RuntimeControlCommand, RuntimeControlReceipt, RuntimeEventCursorError,
    RuntimeEventCursorErrorCode, RuntimeEventCursorPage, RuntimeEventCursorRequest,
    RuntimeEventCursorState, RuntimeEventStreamItem, RuntimeProfile,
};
use crate::{
    LocalApprovalDecision, LocalEvent, LocalMcpInputResolution, LocalRunRecord, LocalRunState,
    LocalRuntimeConfig, LocalRuntimeError, LocalRuntimeHost, local_invocation_context,
};
use agent_protocol::{McpInputResponse, RuntimeInvocationContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use uuid::Uuid;

const LOCAL_DAEMON_MAX_ACTIVE_RUNS: usize = 16;
const LOCAL_DAEMON_MAX_QUEUED_RUNS: usize = 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalRequest {
    /// Start a Run. The connection may close immediately afterwards.
    Submit {
        input: String,
    },
    /// Stream a Run's events, replaying everything after `after_sequence`
    /// before following the live tail.
    Attach {
        run_id: Uuid,
        #[serde(default)]
        after_sequence: u64,
    },
    /// One bounded, versioned page for SDKs and future GUI adapters. Unlike
    /// legacy Attach, all lifecycle and history-gap outcomes remain typed.
    EventCursor {
        request: RuntimeEventCursorRequest,
    },
    /// Runs this daemon has started, newest first.
    List,
    /// Answer the approval a parked Run is waiting on.
    Approve {
        run_id: Uuid,
    },
    Deny {
        run_id: Uuid,
    },
    ResolveMcpInput {
        run_id: Uuid,
        input_id: Uuid,
        input_version: u32,
        binding_digest: String,
        responses: std::collections::BTreeMap<String, McpInputResponse>,
    },
    /// Close a parked Run without running the Tool it was waiting on.
    Cancel {
        run_id: Uuid,
    },
    Resume {
        run_id: Uuid,
    },
    /// Full protocol-neutral command for SDKs and future GUI adapters. Legacy
    /// convenience variants above are translated to this exact contract.
    Control {
        command: RuntimeControlCommand,
    },
}

/// What a Run looks like to whoever owns this state root.
///
/// Deliberately not `LocalRunRecord`: that carries owner epochs and the exact
/// shape of a parked approval, which are the host's business. This is the
/// question a person asks of a list -- what was it asked to do, and where did
/// it get to.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OwnerRunSummary {
    pub run_id: Uuid,
    pub invocation: RuntimeInvocationContext,
    pub input: String,
    pub state: OwnerRunState,
}

/// A Run's state as an owner sees it.
///
/// One variant per durable state, deliberately. Folding "a decision has been
/// acknowledged but not yet consumed" into `Running` would read as work in
/// flight when the Run is actually owed a replay, and folding an MCP input
/// wait into an approval wait would send a person looking for a button that
/// is not there. Nothing here is a summary of two things.
///
/// `Cancelling` drops the operator's reason text on purpose -- that is prose
/// somebody typed, and a status field is not where it belongs. `Interrupted`
/// keeps its reason because the Runtime wrote it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum OwnerRunState {
    Running,
    Cancelling,
    WaitingApproval,
    WaitingInput,
    /// A decision is durable but the exact Checkpoint-bound call has not
    /// consumed it yet. Still owed work, and never resumed by asking the model.
    Decided,
    Interrupted {
        reason: String,
        cause: crate::RunInterruptCause,
    },
    Finished {
        status: String,
    },
}

impl OwnerRunState {
    fn of(state: &LocalRunState) -> Self {
        match state {
            LocalRunState::Running => Self::Running,
            LocalRunState::Cancelling { .. } => Self::Cancelling,
            LocalRunState::AwaitingApproval { .. } => Self::WaitingApproval,
            LocalRunState::AwaitingMcpInput { .. } => Self::WaitingInput,
            LocalRunState::ApprovalDecided { .. } | LocalRunState::McpInputDecided { .. } => {
                Self::Decided
            }
            LocalRunState::Interrupted { reason, cause } => Self::Interrupted {
                reason: reason.clone(),
                cause: *cause,
            },
            LocalRunState::Cancelled { .. } => Self::Finished {
                status: "cancelled".into(),
            },
            LocalRunState::Finished { status } => Self::Finished {
                status: status.clone(),
            },
        }
    }
}

/// Requests only the owner of this state root may issue.
///
/// Separate from `LocalRequest` because the two ask different kinds of thing:
/// a workload asks the Runtime to do work, an owner asks it to be running, to
/// report itself, or to stop. The privilege boundary already exists -- the
/// socket is created `0o600`, so connecting at all is the credential -- and
/// splitting the enum is what keeps the two surfaces legible and lets a test
/// assert that neither namespace can reach the other.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OwnerRequest {
    /// Every Run on disk, newest first.
    ///
    /// Distinct from `LocalRequest::List`, which reports the order this daemon
    /// has started things in since it came up and is therefore empty after a
    /// restart while the Runs are still there. The difference is declared, not
    /// accidental: one answers "what has this host done", the other "what does
    /// this state root hold".
    ListRuns {
        #[serde(default)]
        after_run_id: Option<Uuid>,
        #[serde(default = "default_owner_page")]
        limit: usize,
    },
    /// Recover every Profile and open for work. Safe to ask twice.
    Start,
    /// Lifecycle, recovery progress, what is in flight, and -- once -- what the
    /// previous shutdown left behind.
    Snapshot,
    /// Stop taking work, wait a bounded time for what was admitted, and report.
    Shutdown,
    // Session operations carry no invocation. This daemon owns exactly one
    // state root and one built-in local identity, so asking a caller to supply
    // it would only invite it to supply the wrong one -- and would force every
    // client to mirror seven constants it has no way to choose between.
    SessionStart {
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
        input: String,
    },
    SessionContinue {
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: String,
    },
    SessionFork {
        session_id: Uuid,
        source_branch_id: Uuid,
        source_generation: u64,
        through_turn_ordinal: u64,
        target_branch_id: Uuid,
    },
    SessionRollback {
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        through_turn_ordinal: u64,
    },
    SessionRead {
        session_id: Uuid,
        branch_id: Uuid,
    },
    SessionList {
        #[serde(default)]
        after_session_id: Option<Uuid>,
        #[serde(default)]
        after_branch_id: Option<Uuid>,
        #[serde(default = "default_owner_page")]
        limit: usize,
    },
    SessionHistory {
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        #[serde(default)]
        after_turn_ordinal: u64,
        #[serde(default = "default_owner_history_page")]
        limit: usize,
    },
}

impl OwnerRequest {
    /// Same rule as the workload surface, and the lifecycle operations are not
    /// mutations of work: `start` is what makes the Runtime ready, and
    /// `shutdown` has to be reachable from a Runtime that never became ready.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        match self {
            Self::SessionStart { .. }
            | Self::SessionContinue { .. }
            | Self::SessionFork { .. }
            | Self::SessionRollback { .. } => true,
            Self::ListRuns { .. }
            | Self::SessionRead { .. }
            | Self::SessionList { .. }
            | Self::SessionHistory { .. }
            | Self::Start
            | Self::Snapshot
            | Self::Shutdown => false,
        }
    }
}

const OWNER_MAX_PAGE: usize = 256;
const OWNER_MAX_HISTORY_PAGE: usize = 128;

fn default_owner_page() -> usize {
    OWNER_MAX_PAGE
}

fn default_owner_history_page() -> usize {
    OWNER_MAX_HISTORY_PAGE
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OwnerResponse {
    Runs {
        runs: Vec<OwnerRunSummary>,
        next_after_run_id: Option<Uuid>,
    },
    SessionTurn {
        receipt: Box<crate::embedded::EmbeddedSessionTurnReceipt>,
    },
    SessionHead {
        head: Box<crate::LocalSessionHead>,
    },
    SessionList {
        page: Box<crate::embedded::EmbeddedSessionListPage>,
    },
    SessionHistory {
        page: Box<crate::embedded::EmbeddedSessionHistoryPage>,
    },
    Started,
    /// Same fact as `LocalResponse::NotReady`, for the owner surface.
    NotReady {
        lifecycle: crate::controller::RuntimeLifecycle,
        recovery: crate::controller::RuntimeRecoveryProgress,
    },
    Snapshot {
        lifecycle: crate::controller::RuntimeLifecycle,
        recovery: crate::controller::RuntimeRecoveryProgress,
        active_runs: usize,
        queued_runs: usize,
        recovery_failures: usize,
        previous_shutdown: Option<crate::controller::RuntimeShutdownReport>,
    },
    Shutdown {
        report: Box<crate::controller::RuntimeShutdownReport>,
    },
    Error {
        message: String,
    },
}

/// Which surface a line on the socket is addressed to.
///
/// A line without a scope is a workload request, so every existing client keeps
/// working unchanged. A line that names a scope is only ever parsed as that
/// scope: there is no fallback from one namespace into the other, because a
/// fallback is exactly how an owner operation would become reachable by
/// something that did not ask for one.
// Debug adds nothing `LocalRequest` and `OwnerRequest` do not already derive;
// it exists so a test can say which namespace it got.
#[derive(Debug)]
enum WireRequest {
    Workload(LocalRequest),
    Owner(OwnerRequest),
}

fn parse_wire_request(line: &str) -> Result<WireRequest, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
    match value.get("scope").and_then(serde_json::Value::as_str) {
        Some("owner") => serde_json::from_value(value)
            .map(WireRequest::Owner)
            .map_err(|error| error.to_string()),
        Some(other) if other != "workload" => Err(format!("unknown request scope {other}")),
        _ => serde_json::from_value(value)
            .map(WireRequest::Workload)
            .map_err(|error| error.to_string()),
    }
}

impl LocalRequest {
    /// Whether this asks the Runtime to change something.
    ///
    /// Written out per variant rather than as a default-plus-exceptions: a
    /// mutation that is added later and forgotten here would be accepted while
    /// the Runtime is still recovering or already draining, which is the exact
    /// window this exists to close.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        match self {
            Self::Submit { .. }
            | Self::Approve { .. }
            | Self::Deny { .. }
            | Self::ResolveMcpInput { .. }
            | Self::Cancel { .. }
            | Self::Resume { .. }
            | Self::Control { .. } => true,
            Self::Attach { .. } | Self::EventCursor { .. } | Self::List => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalResponse {
    Accepted {
        run_id: Uuid,
    },
    Event {
        event: Box<LocalEvent>,
    },
    Finished {
        run_id: Uuid,
        status: String,
    },
    Runs {
        run_ids: Vec<Uuid>,
    },
    ControlReceipt {
        receipt: Box<RuntimeControlReceipt>,
    },
    EventCursor {
        page: Box<RuntimeEventCursorPage>,
    },
    EventCursorError {
        error: RuntimeEventCursorError,
    },
    /// The Runtime is not open for work yet, and this is not a failure.
    ///
    /// Its own reply rather than an error string: recovering and not running
    /// are different states, and a client that cannot tell them apart reports
    /// a healthy startup as a fault. Carrying the progress is what lets it say
    /// "restoring 12 of 40" instead of "cannot connect".
    NotReady {
        lifecycle: crate::controller::RuntimeLifecycle,
        recovery: crate::controller::RuntimeRecoveryProgress,
    },
    Error {
        message: String,
    },
}

pub struct LocalRuntimeDaemon {
    config: LocalRuntimeConfig,
    invocation: RuntimeInvocationContext,
    runtime: Arc<EmbeddedRuntime>,
    /// The owner-side lifecycle. Held here so that stopping the Runtime is
    /// something a separate-process client can ask for over this socket -- an
    /// in-process Controller is reachable from Tauri and from nothing that runs
    /// in another process, which is most of what will drive this.
    controller: Arc<crate::controller::RuntimeController>,
    order: Arc<Mutex<Vec<Uuid>>>,
}

impl LocalRuntimeDaemon {
    #[must_use]
    pub fn new(config: LocalRuntimeConfig) -> Arc<Self> {
        Self::new_for_invocation(config, local_invocation_context())
            .expect("the built-in local invocation identity is valid")
    }

    pub fn new_for_invocation(
        config: LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
    ) -> Result<Arc<Self>, LocalRuntimeError> {
        invocation
            .validate()
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
        Self::migrate_legacy_local_records(&config, invocation)?;
        let runtime = EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: LOCAL_DAEMON_MAX_ACTIVE_RUNS,
                max_active_runs_per_tenant: LOCAL_DAEMON_MAX_ACTIVE_RUNS,
                max_active_runs_per_workspace: 1,
                max_queued_runs: LOCAL_DAEMON_MAX_QUEUED_RUNS,
                max_queued_runs_per_tenant: LOCAL_DAEMON_MAX_QUEUED_RUNS,
            },
            vec![RuntimeProfile {
                invocation,
                config: config.clone(),
            }],
        )
        .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
        let runtime = Arc::new(runtime);
        Ok(Arc::new(Self {
            config,
            invocation,
            controller: crate::controller::RuntimeController::new(Arc::clone(&runtime)),
            runtime,
            order: Arc::new(Mutex::new(Vec::new())),
        }))
    }

    /// The Runtime this daemon drives, so a second adapter can be served from
    /// the same process without a second `EmbeddedRuntime`.
    ///
    /// One Runtime per state root is not a convenience: two would each believe
    /// they owned the same directory, and the admission ceilings, owner epochs
    /// and retention gates that keep a state root consistent are per-instance.
    #[must_use]
    pub fn runtime(&self) -> Arc<EmbeddedRuntime> {
        Arc::clone(&self.runtime)
    }

    /// The invocation this daemon was registered for. A network adapter needs
    /// it to state which Profile exists, and nothing else.
    #[must_use]
    pub const fn invocation(&self) -> RuntimeInvocationContext {
        self.invocation
    }

    fn record_is_owned(&self, record: &LocalRunRecord) -> bool {
        let legacy = [
            record.tenant_id,
            record.application_id,
            record.workload_identity_id,
            record.workspace_id,
            record.agent_version_id,
            record.model_policy_id,
        ]
        .iter()
        .all(Uuid::is_nil);
        if legacy {
            return self.invocation == local_invocation_context();
        }
        record.tenant_id == self.invocation.tenant_id
            && record.application_id == self.invocation.application_id
            && record.workload_identity_id == self.invocation.workload_identity_id
            && record.workspace_id == self.invocation.workspace_id
            && record.agent_version_id == self.invocation.agent_version_id
            && record.model_policy_id == self.invocation.model_policy_id
    }

    fn migrate_legacy_local_records(
        config: &LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
    ) -> Result<(), LocalRuntimeError> {
        if invocation != local_invocation_context() {
            return Ok(());
        }
        for record in LocalRuntimeHost::list_run_records(&config.state_root)? {
            let legacy = [
                record.tenant_id,
                record.application_id,
                record.workload_identity_id,
                record.workspace_id,
                record.agent_version_id,
                record.model_policy_id,
            ]
            .iter()
            .all(Uuid::is_nil);
            if legacy {
                LocalRuntimeHost::write_run_record(
                    &config.state_root,
                    &LocalRunRecord {
                        tenant_id: invocation.tenant_id,
                        application_id: invocation.application_id,
                        workload_identity_id: invocation.workload_identity_id,
                        workspace_id: invocation.workspace_id,
                        agent_version_id: invocation.agent_version_id,
                        model_policy_id: invocation.model_policy_id,
                        ..record
                    },
                )?;
            }
        }
        Ok(())
    }

    fn read_owned_record(&self, run_id: Uuid) -> Result<Option<LocalRunRecord>, LocalRuntimeError> {
        self.runtime
            .read_run_record(self.invocation, run_id)
            .map_err(Self::map_runtime_error)
    }

    /// Releases the control socket on a clean shutdown. Removing it after the
    /// listener is gone means the next start sees nothing to reason about; a
    /// socket left behind is indistinguishable from a live one until probed.
    pub fn release(socket_path: &Path, listener: UnixListener) {
        drop(listener);
        let _ = std::fs::remove_file(socket_path);
    }

    /// Binds the control socket. The socket is created with owner-only
    /// permissions because whoever can talk to it can spend the provider
    /// credential this host holds.
    pub async fn bind(socket_path: &Path) -> Result<UnixListener, LocalRuntimeError> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
        // A socket left by a crashed host and a socket held by a live one look
        // identical on disk. Deleting either unconditionally -- which is what
        // this used to do -- let a second daemon take a live state root, and
        // then two hosts owned the same Runs and the same durable records. A
        // desktop client started twice is the ordinary way to reach that.
        //
        // Connecting is the only way to tell the two apart. If something
        // answers, refuse; if nothing does, the file is debris and is removed.
        if socket_path.exists() {
            match UnixStream::connect(socket_path).await {
                Ok(stream) => {
                    drop(stream);
                    return Err(LocalRuntimeError::AlreadyRunning(
                        socket_path.display().to_string(),
                    ));
                }
                Err(_) => {
                    std::fs::remove_file(socket_path)
                        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
                }
            }
        }
        let listener = UnixListener::bind(socket_path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
        Ok(listener)
    }

    /// Accepts connections, and brings the Runtime up while it does.
    ///
    /// Recovery runs concurrently with accepting rather than before it. The
    /// constraint that matters -- no window in which new work is admitted
    /// before recovery finishes -- is held by the mutation gate, not by
    /// refusing to answer the door. A socket that does not answer during
    /// startup makes a client report a healthy Runtime as unreachable, and a
    /// desktop application with Runs to restore would do that on every launch.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        let starting = Arc::clone(&self);
        tokio::spawn(async move {
            let _ = starting.controller.start().await;
        });
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let daemon = Arc::clone(&self);
            // Each connection is independent; losing one never touches a Run.
            tokio::spawn(async move {
                let _ = daemon.handle_connection(stream).await;
            });
        }
    }

    /// Stops taking work, drains within the deadline, and reports.
    ///
    /// The single closing path. A signal, the owner socket and a test all call
    /// this, because a second implementation of "stop" is a second set of rules
    /// about what a stopped Runtime leaves behind.
    pub async fn shutdown(&self) -> crate::controller::RuntimeShutdownReport {
        self.controller.shutdown().await
    }

    /// Blocks until the Runtime is open for work, or has stopped trying.
    ///
    /// For callers that genuinely need to wait -- a test, or a launcher that
    /// wants to report readiness. A client on the socket does not need this:
    /// it is told `NotReady` and can decide for itself.
    pub async fn wait_until_ready(&self) {
        let _ = self.controller.start().await;
    }

    /// Whether work may be accepted right now.
    ///
    /// Reads stay available throughout: a client watching a Runtime come up, or
    /// looking at what a stopped one left behind, is not asking it to do
    /// anything. Only mutations wait for `Ready`.
    async fn accepting_work(
        &self,
    ) -> Option<(
        crate::controller::RuntimeLifecycle,
        crate::controller::RuntimeRecoveryProgress,
    )> {
        let snapshot = self.controller.snapshot().await;
        if snapshot.lifecycle == crate::controller::RuntimeLifecycle::Ready {
            return None;
        }
        Some((snapshot.lifecycle, snapshot.recovery))
    }

    /// Every Run this state root holds, newest first, from disk.
    ///
    /// `LocalRequest::List` answers a different question -- what this daemon
    /// has started since it came up -- and is empty after a restart while the
    /// Runs are still on disk. Both are useful and neither is a substitute for
    /// the other, so they stay separate rather than one quietly changing
    /// meaning.
    fn list_owned_runs(
        &self,
        after_run_id: Option<Uuid>,
        limit: usize,
    ) -> Result<(Vec<OwnerRunSummary>, Option<Uuid>), LocalRuntimeError> {
        if !(1..=OWNER_MAX_PAGE).contains(&limit) {
            return Err(LocalRuntimeError::Execution(format!(
                "owner page limit must be between 1 and {OWNER_MAX_PAGE}"
            )));
        }
        let mut records = LocalRuntimeHost::list_run_records(&self.config.state_root)?;
        // Run ids are UUIDv7, so descending id is newest first and is also a
        // stable paging order -- no timestamp to tie-break and no scan state to
        // keep between pages.
        records.sort_by_key(|record| std::cmp::Reverse(record.run_id));
        let start = match after_run_id {
            Some(cursor) => records
                .iter()
                .position(|record| record.run_id == cursor)
                .map(|at| at + 1)
                .ok_or_else(|| {
                    LocalRuntimeError::Execution("owner page cursor is not a known Run".into())
                })?,
            None => 0,
        };
        let page: Vec<_> = records
            .into_iter()
            .skip(start)
            .take(limit)
            .map(|record| OwnerRunSummary {
                run_id: record.run_id,
                invocation: RuntimeInvocationContext {
                    schema_version: agent_protocol::RUNTIME_INVOCATION_SCHEMA_VERSION,
                    tenant_id: record.tenant_id,
                    application_id: record.application_id,
                    workload_identity_id: record.workload_identity_id,
                    workspace_id: record.workspace_id,
                    agent_version_id: record.agent_version_id,
                    model_policy_id: record.model_policy_id,
                },
                state: OwnerRunState::of(&record.state),
                input: record.input,
            })
            .collect();
        let next = (page.len() == limit)
            .then(|| page.last().map(|summary| summary.run_id))
            .flatten();
        Ok((page, next))
    }

    async fn handle_connection(self: Arc<Self>, stream: UnixStream) -> std::io::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request = match parse_wire_request(&line) {
                Ok(WireRequest::Workload(request)) => request,
                Ok(WireRequest::Owner(request)) => {
                    if request.is_mutation()
                        && let Some((lifecycle, recovery)) = self.accepting_work().await
                    {
                        write_owner_response(
                            &mut writer,
                            &OwnerResponse::NotReady {
                                lifecycle,
                                recovery,
                            },
                        )
                        .await?;
                        continue;
                    }
                    let response = match request {
                        OwnerRequest::ListRuns {
                            after_run_id,
                            limit,
                        } => match self.list_owned_runs(after_run_id, limit) {
                            Ok((runs, next_after_run_id)) => OwnerResponse::Runs {
                                runs,
                                next_after_run_id,
                            },
                            Err(error) => OwnerResponse::Error {
                                message: error.to_string(),
                            },
                        },
                        OwnerRequest::Start => match self.controller.start().await {
                            Ok(()) => OwnerResponse::Started,
                            Err(error) => OwnerResponse::Error {
                                message: error.to_string(),
                            },
                        },
                        OwnerRequest::Snapshot => {
                            let snapshot = self.controller.snapshot().await;
                            OwnerResponse::Snapshot {
                                lifecycle: snapshot.lifecycle,
                                recovery: snapshot.recovery,
                                active_runs: snapshot.active_runs,
                                queued_runs: snapshot.queued_runs,
                                // A count, not the failures themselves: a
                                // client that needs to act on one asks the
                                // Profile, and a socket reply is not the place
                                // to fan out per-tenant diagnostics.
                                recovery_failures: snapshot.recovery_failures.len(),
                                previous_shutdown: snapshot.previous_shutdown,
                            }
                        }
                        OwnerRequest::Shutdown => OwnerResponse::Shutdown {
                            report: Box::new(self.controller.shutdown().await),
                        },
                        OwnerRequest::SessionStart {
                            session_id,
                            branch_id,
                            run_id,
                            input,
                        } => owner_turn(
                            self.runtime
                                .start_session_turn_detached(
                                    self.invocation,
                                    session_id,
                                    branch_id,
                                    run_id,
                                    input,
                                )
                                .await,
                        ),
                        OwnerRequest::SessionContinue {
                            session_id,
                            branch_id,
                            generation,
                            run_id,
                            input,
                        } => owner_turn(
                            self.runtime
                                .continue_session_turn_detached(
                                    self.invocation,
                                    session_id,
                                    branch_id,
                                    generation,
                                    run_id,
                                    input,
                                )
                                .await,
                        ),
                        OwnerRequest::SessionFork {
                            session_id,
                            source_branch_id,
                            source_generation,
                            through_turn_ordinal,
                            target_branch_id,
                        } => owner_head(
                            self.runtime
                                .fork_session(
                                    self.invocation,
                                    session_id,
                                    source_branch_id,
                                    source_generation,
                                    through_turn_ordinal,
                                    target_branch_id,
                                )
                                .await,
                        ),
                        OwnerRequest::SessionRollback {
                            session_id,
                            branch_id,
                            generation,
                            through_turn_ordinal,
                        } => owner_head(
                            self.runtime
                                .rollback_session(
                                    self.invocation,
                                    session_id,
                                    branch_id,
                                    generation,
                                    through_turn_ordinal,
                                )
                                .await,
                        ),
                        OwnerRequest::SessionRead {
                            session_id,
                            branch_id,
                        } => owner_head(self.runtime.read_session_head(
                            self.invocation,
                            session_id,
                            branch_id,
                        )),
                        OwnerRequest::SessionList {
                            after_session_id,
                            after_branch_id,
                            limit,
                        } => {
                            // Both halves of the cursor or neither. A Session
                            // named without its branch is an incomplete cursor,
                            // not a shorthand for "any branch".
                            match (after_session_id, after_branch_id) {
                                (Some(session_id), Some(branch_id)) => {
                                    owner_list(self.runtime.list_session_heads(
                                        self.invocation,
                                        Some((session_id, branch_id)),
                                        limit,
                                    ))
                                }
                                (None, None) => owner_list(self.runtime.list_session_heads(
                                    self.invocation,
                                    None,
                                    limit,
                                )),
                                _ => OwnerResponse::Error {
                                    message: "a Session list cursor needs both its Session and \
                                              its branch, or neither"
                                        .into(),
                                },
                            }
                        }
                        OwnerRequest::SessionHistory {
                            session_id,
                            branch_id,
                            generation,
                            after_turn_ordinal,
                            limit,
                        } => match self.runtime.read_session_history(
                            self.invocation,
                            session_id,
                            branch_id,
                            generation,
                            after_turn_ordinal,
                            limit,
                        ) {
                            Ok(page) => OwnerResponse::SessionHistory {
                                page: Box::new(page),
                            },
                            Err(error) => OwnerResponse::Error {
                                message: error.to_string(),
                            },
                        },
                    };
                    write_owner_response(&mut writer, &response).await?;
                    continue;
                }
                Err(message) => {
                    write_response(&mut writer, &LocalResponse::Error { message }).await?;
                    continue;
                }
            };
            if request.is_mutation()
                && let Some((lifecycle, recovery)) = self.accepting_work().await
            {
                write_response(
                    &mut writer,
                    &LocalResponse::NotReady {
                        lifecycle,
                        recovery,
                    },
                )
                .await?;
                continue;
            }
            match request {
                LocalRequest::Submit { input } => {
                    let response = match self.spawn_run(input).await {
                        Ok(run_id) => LocalResponse::Accepted { run_id },
                        Err(error) => LocalResponse::Error {
                            message: error.to_string(),
                        },
                    };
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::List => {
                    let run_ids = self.order.lock().await.iter().rev().copied().collect();
                    write_response(&mut writer, &LocalResponse::Runs { run_ids }).await?;
                }
                LocalRequest::Attach {
                    run_id,
                    after_sequence,
                } => {
                    self.stream_run(&mut writer, run_id, after_sequence).await?;
                }
                LocalRequest::EventCursor { request } => {
                    let response = match self.runtime.event_cursor(request) {
                        Ok(page) => LocalResponse::EventCursor {
                            page: Box::new(page),
                        },
                        Err(EmbeddedRuntimeError::EventCursor(error)) => {
                            LocalResponse::EventCursorError { error }
                        }
                        Err(EmbeddedRuntimeError::UnregisteredInvocation) => {
                            LocalResponse::EventCursorError {
                                error: RuntimeEventCursorError {
                                    code: RuntimeEventCursorErrorCode::IdentityMismatch,
                                    message: "Runtime event invocation is not registered".into(),
                                },
                            }
                        }
                        Err(_) => LocalResponse::EventCursorError {
                            error: RuntimeEventCursorError {
                                code: RuntimeEventCursorErrorCode::StorageUnavailable,
                                message: "Runtime event cursor is unavailable".into(),
                            },
                        },
                    };
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::Approve { run_id } => {
                    let response = self.decide(run_id, LocalApprovalDecision::AllowOnce).await;
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::Deny { run_id } => {
                    let response = self.decide(run_id, LocalApprovalDecision::Deny).await;
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::ResolveMcpInput {
                    run_id,
                    input_id,
                    input_version,
                    binding_digest,
                    responses,
                } => {
                    let response = self
                        .resolve_mcp_input(
                            LocalMcpInputResolution {
                                input_id,
                                input_version,
                                binding_digest,
                                responses,
                            },
                            run_id,
                        )
                        .await;
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::Cancel { run_id } => {
                    let response = self.cancel(run_id).await;
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::Resume { run_id } => {
                    let response = self.resume(run_id).await;
                    write_response(&mut writer, &response).await?;
                }
                LocalRequest::Control { command } => {
                    let response = match self.runtime.control_detached(command).await {
                        Ok(receipt) => LocalResponse::ControlReceipt {
                            receipt: Box::new(receipt),
                        },
                        Err(error) => LocalResponse::Error {
                            message: error.to_string(),
                        },
                    };
                    write_response(&mut writer, &response).await?;
                }
            }
        }
        Ok(())
    }

    /// Starts a Run through the same embedded coordinator used by every other
    /// adapter. The durable Run record exists before acceptance is returned.
    async fn spawn_run(self: &Arc<Self>, input: String) -> Result<Uuid, EmbeddedRuntimeError> {
        let run_id = Uuid::now_v7();
        self.runtime
            .execute_detached(self.invocation, run_id, input)
            .await?;
        let mut order = self.order.lock().await;
        if !order.contains(&run_id) {
            order.push(run_id);
        }
        Ok(run_id)
    }

    fn map_runtime_error(error: EmbeddedRuntimeError) -> LocalRuntimeError {
        match error {
            EmbeddedRuntimeError::Runtime(error) => error,
            other => LocalRuntimeError::Configuration(other.to_string()),
        }
    }

    fn legacy_command_id(run_id: Uuid, kind: &str, related_id: Option<Uuid>) -> Uuid {
        let mut digest = Sha256::new();
        digest.update(b"agent-runtime-local-control-v1\0");
        digest.update(run_id.as_bytes());
        digest.update(kind.as_bytes());
        if let Some(related_id) = related_id {
            digest.update(related_id.as_bytes());
        }
        let digest = digest.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        // RFC 9562 custom UUID (version 8), with the standard variant.
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes)
    }

    fn existing_legacy_command(
        &self,
        command_id: Uuid,
    ) -> Result<Option<RuntimeControlCommand>, EmbeddedRuntimeError> {
        Ok(self
            .runtime
            .read_control_receipt(self.invocation, command_id)?
            .map(|receipt| receipt.command()))
    }

    async fn dispatch_legacy_control(
        self: &Arc<Self>,
        command: RuntimeControlCommand,
    ) -> LocalResponse {
        match self.runtime.control_detached(command).await {
            Ok(receipt) => LocalResponse::Accepted {
                run_id: receipt.run_id,
            },
            Err(error) => LocalResponse::Error {
                message: error.to_string(),
            },
        }
    }

    /// Legacy approve/deny remains a convenience adapter, but the exact pending
    /// approval and stable command id are converted into RuntimeControlCommand.
    async fn decide(
        self: &Arc<Self>,
        run_id: Uuid,
        decision: LocalApprovalDecision,
    ) -> LocalResponse {
        let kind = match decision {
            LocalApprovalDecision::AllowOnce => "approve",
            LocalApprovalDecision::Deny => "deny",
        };
        let command_id = Self::legacy_command_id(run_id, kind, None);
        match self.existing_legacy_command(command_id) {
            Ok(Some(command))
                if matches!(
                    command.action,
                    RuntimeControlAction::DecideApproval {
                        decision: recorded,
                        ..
                    } if recorded == decision
                ) =>
            {
                return self.dispatch_legacy_control(command).await;
            }
            Ok(Some(_)) => {
                return LocalResponse::Error {
                    message: "legacy approval command id is bound to another action".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
            Ok(None) => {}
        }
        let record = match self.read_owned_record(run_id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return LocalResponse::Error {
                    message: "unknown run".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
        };
        let (target_run_id, approval_id, binding_digest) = match &record.state {
            LocalRunState::AwaitingApproval {
                approval_id,
                binding_digest,
                target_run_id,
            } => (
                target_run_id.unwrap_or(run_id),
                *approval_id,
                binding_digest.clone(),
            ),
            LocalRunState::ApprovalDecided {
                target_run_id,
                approval_id,
                binding_digest,
                decision: recorded,
            } if *recorded == decision => (*target_run_id, *approval_id, binding_digest.clone()),
            LocalRunState::ApprovalDecided { .. } => {
                return LocalResponse::Error {
                    message: "approval was already decided differently".into(),
                };
            }
            state => {
                return LocalResponse::Error {
                    message: format!("run is not awaiting approval: {state:?}"),
                };
            }
        };
        self.dispatch_legacy_control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id,
            invocation: self.invocation,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::DecideApproval {
                target_run_id,
                approval_id,
                binding_digest,
                decision,
            },
        })
        .await
    }

    async fn resolve_mcp_input(
        self: &Arc<Self>,
        resolution: LocalMcpInputResolution,
        run_id: Uuid,
    ) -> LocalResponse {
        let command_id =
            Self::legacy_command_id(run_id, "resolve_mcp_input", Some(resolution.input_id));
        match self.existing_legacy_command(command_id) {
            Ok(Some(command))
                if matches!(
                    &command.action,
                    RuntimeControlAction::ResolveMcpInput { input_id, .. }
                        if *input_id == resolution.input_id
                ) =>
            {
                return self.dispatch_legacy_control(command).await;
            }
            Ok(Some(_)) => {
                return LocalResponse::Error {
                    message: "legacy MCP input command id is bound to another action".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
            Ok(None) => {}
        }
        let record = match self.read_owned_record(run_id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return LocalResponse::Error {
                    message: "unknown run".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
        };
        match &record.state {
            LocalRunState::AwaitingMcpInput { input }
                if input.input_id == resolution.input_id
                    && input.binding_digest == resolution.binding_digest => {}
            LocalRunState::McpInputDecided {
                resolution: recorded,
            } if recorded == &resolution => {}
            LocalRunState::McpInputDecided { .. } => {
                return LocalResponse::Error {
                    message: "MCP input was already answered differently".into(),
                };
            }
            state => {
                return LocalResponse::Error {
                    message: format!("run is not awaiting MCP input: {state:?}"),
                };
            }
        }
        self.dispatch_legacy_control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id,
            invocation: self.invocation,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::ResolveMcpInput {
                input_id: resolution.input_id,
                input_version: resolution.input_version,
                binding_digest: resolution.binding_digest,
                responses: resolution.responses,
            },
        })
        .await
    }

    async fn cancel(self: &Arc<Self>, run_id: Uuid) -> LocalResponse {
        let command_id = Self::legacy_command_id(run_id, "cancel", None);
        match self.existing_legacy_command(command_id) {
            Ok(Some(command)) if matches!(command.action, RuntimeControlAction::Cancel { .. }) => {
                return self.dispatch_legacy_control(command).await;
            }
            Ok(Some(_)) => {
                return LocalResponse::Error {
                    message: "legacy cancellation command id is bound to another action".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
            Ok(None) => {}
        }
        let record = match self.read_owned_record(run_id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return LocalResponse::Error {
                    message: "unknown run".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
        };
        self.dispatch_legacy_control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id,
            invocation: self.invocation,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Cancel {
                reason: "cancelled by the local operator".into(),
            },
        })
        .await
    }

    async fn resume(self: &Arc<Self>, run_id: Uuid) -> LocalResponse {
        let command_id = Self::legacy_command_id(run_id, "resume", None);
        match self.existing_legacy_command(command_id) {
            Ok(Some(command)) if matches!(command.action, RuntimeControlAction::Resume) => {
                return self.dispatch_legacy_control(command).await;
            }
            Ok(Some(_)) => {
                return LocalResponse::Error {
                    message: "legacy resume command id is bound to another action".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
            Ok(None) => {}
        }
        let record = match self.read_owned_record(run_id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                return LocalResponse::Error {
                    message: "unknown run".into(),
                };
            }
            Err(error) => {
                return LocalResponse::Error {
                    message: error.to_string(),
                };
            }
        };
        self.dispatch_legacy_control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id,
            invocation: self.invocation,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Resume,
        })
        .await
    }

    pub async fn recover_unfinished(self: &Arc<Self>) -> Result<usize, LocalRuntimeError> {
        let recovered = self
            .runtime
            .recover_unfinished_detached(self.invocation)
            .await
            .map_err(Self::map_runtime_error)?;
        let mut run_ids = LocalRuntimeHost::list_run_records(&self.config.state_root)?
            .into_iter()
            .filter(|record| self.record_is_owned(record))
            .map(|record| record.run_id)
            .collect::<Vec<_>>();
        run_ids.sort_unstable();
        run_ids.dedup();
        *self.order.lock().await = run_ids;
        Ok(recovered)
    }
    /// Compatibility stream backed by the bounded versioned cursor. No call
    /// reads the complete log into memory; new SDKs should use EventCursor so
    /// terminal, retired and history-gap states remain typed on the wire.
    async fn stream_run(
        self: &Arc<Self>,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        run_id: Uuid,
        after_sequence: u64,
    ) -> std::io::Result<()> {
        let mut subscription =
            match self
                .runtime
                .subscribe_events(self.invocation, run_id, after_sequence, 64)
            {
                Ok(subscription) => subscription,
                Err(error) => {
                    write_response(
                        writer,
                        &LocalResponse::Error {
                            message: error.to_string(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
        while let Some(item) = subscription.recv().await {
            match item {
                Ok(RuntimeEventStreamItem::Event { event, .. }) => {
                    write_response(writer, &LocalResponse::Event { event }).await?;
                }
                Ok(RuntimeEventStreamItem::Boundary {
                    state, history_gap, ..
                }) => {
                    if history_gap {
                        write_response(
                            writer,
                            &LocalResponse::Error {
                                message: "Run event history was retired before this cursor".into(),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    let status = match state {
                        RuntimeEventCursorState::Terminal { status }
                        | RuntimeEventCursorState::Retired { status, .. } => {
                            status.as_str().to_owned()
                        }
                        RuntimeEventCursorState::WaitingApproval => "waiting_approval".into(),
                        RuntimeEventCursorState::Suspended => "suspended".into(),
                        RuntimeEventCursorState::Interrupted => "interrupted".into(),
                        RuntimeEventCursorState::Running | RuntimeEventCursorState::Cancelling => {
                            return Ok(());
                        }
                    };
                    write_response(writer, &LocalResponse::Finished { run_id, status }).await?;
                    return Ok(());
                }
                Err(error) => {
                    write_response(
                        writer,
                        &LocalResponse::Error {
                            message: error.to_string(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &LocalResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

fn owner_turn(
    result: Result<crate::embedded::EmbeddedSessionTurnReceipt, EmbeddedRuntimeError>,
) -> OwnerResponse {
    match result {
        Ok(receipt) => OwnerResponse::SessionTurn {
            receipt: Box::new(receipt),
        },
        Err(error) => OwnerResponse::Error {
            message: error.to_string(),
        },
    }
}

fn owner_head(result: Result<crate::LocalSessionHead, EmbeddedRuntimeError>) -> OwnerResponse {
    match result {
        Ok(head) => OwnerResponse::SessionHead {
            head: Box::new(head),
        },
        Err(error) => OwnerResponse::Error {
            message: error.to_string(),
        },
    }
}

fn owner_list(
    result: Result<crate::embedded::EmbeddedSessionListPage, EmbeddedRuntimeError>,
) -> OwnerResponse {
    match result {
        Ok(page) => OwnerResponse::SessionList {
            page: Box::new(page),
        },
        Err(error) => OwnerResponse::Error {
            message: error.to_string(),
        },
    }
}

/// The owner namespace answers on the same connection and in the same framing,
/// but with its own response type -- an owner reply is never mistaken for a
/// workload one by a client reading either.
async fn write_owner_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &OwnerResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Longest usable Unix socket path. `sockaddr_un.sun_path` holds 104 bytes on
/// macOS and 108 on Linux, including the trailing NUL; 100 is safe on both.
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Control socket for a state root.
///
/// Normally it lives inside the state root, which keeps everything about a
/// local host in one directory. A state root of ordinary desktop depth --
/// `~/Library/Application Support/<vendor>/<app>/<profile>` -- overflows
/// `sun_path`, so an overlong path falls back to a deterministic name in the
/// per-user temp directory. Both the daemon and its clients call this function,
/// so they always agree on where the socket is.
#[must_use]
pub fn default_socket_path(state_root: &Path) -> PathBuf {
    let inside = state_root.join("runtime-host.sock");
    if inside.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return inside;
    }
    let digest = hex::encode(Sha256::digest(state_root.as_os_str().as_encoded_bytes()));
    std::env::temp_dir().join(format!("agent-runtime-host-{}.sock", &digest[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two namespaces do not reach each other, in either direction.
    ///
    /// This is the whole point of splitting the enum. The socket is already
    /// `0o600`, so this is not what keeps a stranger out -- it is what keeps
    /// the two surfaces from quietly becoming one, which is how an owner
    /// operation ends up reachable from a request that never asked to be one.
    #[test]
    fn a_scope_is_never_parsed_as_the_other_one() {
        // Every existing client sends no scope at all and must keep working.
        assert!(matches!(
            parse_wire_request(r#"{"type":"list"}"#).expect("unscoped is workload"),
            WireRequest::Workload(LocalRequest::List)
        ));
        assert!(matches!(
            parse_wire_request(r#"{"scope":"workload","type":"list"}"#).expect("explicit workload"),
            WireRequest::Workload(LocalRequest::List)
        ));
        assert!(matches!(
            parse_wire_request(r#"{"scope":"owner","type":"list_runs"}"#).expect("owner"),
            WireRequest::Owner(OwnerRequest::ListRuns { .. })
        ));

        // A workload operation named under the owner scope is not a workload
        // operation. There is no fallback out of the namespace it named.
        parse_wire_request(r#"{"scope":"owner","type":"list"}"#)
            .expect_err("a workload operation must not be reachable through the owner scope");
        parse_wire_request(r#"{"scope":"owner","type":"submit","input":"hello"}"#)
            .expect_err("submit is not an owner operation");

        // And an owner operation is not reachable by omitting the scope, which
        // is the direction that matters: every existing client omits it.
        parse_wire_request(r#"{"type":"list_runs"}"#)
            .expect_err("an owner operation must not be reachable without naming the owner scope");

        // A scope nobody defined is refused rather than guessed at.
        parse_wire_request(r#"{"scope":"admin","type":"list"}"#)
            .expect_err("an unknown scope must be refused");
    }

    /// A page ceiling the caller can read is a ceiling the caller can plan
    /// against; one it discovers by being refused is a surprise.
    #[test]
    fn the_owner_page_defaults_to_its_published_ceiling() {
        let request = parse_wire_request(r#"{"scope":"owner","type":"list_runs"}"#).expect("owner");
        let WireRequest::Owner(OwnerRequest::ListRuns {
            limit,
            after_run_id,
        }) = request
        else {
            panic!("expected ListRuns");
        };
        assert_eq!(limit, OWNER_MAX_PAGE);
        assert_eq!(after_run_id, None);
    }
}
