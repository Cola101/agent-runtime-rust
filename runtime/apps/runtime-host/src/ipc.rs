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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalResponse {
    Accepted { run_id: Uuid },
    Event { event: Box<LocalEvent> },
    Finished { run_id: Uuid, status: String },
    Runs { run_ids: Vec<Uuid> },
    ControlReceipt { receipt: Box<RuntimeControlReceipt> },
    EventCursor { page: Box<RuntimeEventCursorPage> },
    EventCursorError { error: RuntimeEventCursorError },
    Error { message: String },
}

pub struct LocalRuntimeDaemon {
    config: LocalRuntimeConfig,
    invocation: RuntimeInvocationContext,
    runtime: Arc<EmbeddedRuntime>,
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
        Ok(Arc::new(Self {
            config,
            invocation,
            runtime: Arc::new(runtime),
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

    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
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

    async fn handle_connection(self: Arc<Self>, stream: UnixStream) -> std::io::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let request = match serde_json::from_str::<LocalRequest>(&line) {
                Ok(request) => request,
                Err(error) => {
                    write_response(
                        &mut writer,
                        &LocalResponse::Error {
                            message: error.to_string(),
                        },
                    )
                    .await?;
                    continue;
                }
            };
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
