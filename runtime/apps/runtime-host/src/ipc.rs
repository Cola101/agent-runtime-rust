//! Long-running local host and its Unix-socket IPC (ADR-0035 decision 7).
//!
//! The client is not the Runtime. A Run is owned by the daemon and by the local
//! store, so disconnecting a client must never cancel work, and reconnecting
//! must reconstruct what happened from the durable event log rather than from
//! anything the client remembered.

use crate::{
    LocalApprovalDecision, LocalApprovalResolution, LocalEvent, LocalMcpInputResolution,
    LocalResumeResolution, LocalRunRecord, LocalRunState, LocalRuntimeConfig, LocalRuntimeError,
    LocalRuntimeHost, local_invocation_context,
};
use agent_protocol::{McpInputResponse, RuntimeInvocationContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

/// Bounded live-tail buffer. A slow client that falls behind is told to replay
/// from the log rather than being silently given a hole in the stream.
const LIVE_TAIL_CAPACITY: usize = 1024;

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
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalResponse {
    Accepted { run_id: Uuid },
    Event { event: Box<LocalEvent> },
    Finished { run_id: Uuid, status: String },
    Runs { run_ids: Vec<Uuid> },
    Error { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RunLifecycle {
    Running,
    Cancelling,
    Finished(String),
}

struct RunHandle {
    live: broadcast::Sender<LocalEvent>,
    lifecycle: Arc<Mutex<RunLifecycle>>,
    cancellation: tokio_util::sync::CancellationToken,
}

pub struct LocalRuntimeDaemon {
    config: LocalRuntimeConfig,
    invocation: RuntimeInvocationContext,
    runs: Arc<Mutex<HashMap<Uuid, Arc<RunHandle>>>>,
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
        Ok(Arc::new(Self {
            config,
            invocation,
            runs: Arc::new(Mutex::new(HashMap::new())),
            order: Arc::new(Mutex::new(Vec::new())),
        }))
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

    fn read_owned_record(&self, run_id: Uuid) -> Result<Option<LocalRunRecord>, LocalRuntimeError> {
        Ok(
            LocalRuntimeHost::read_run_record(&self.config.state_root, run_id)?
                .filter(|record| self.record_is_owned(record)),
        )
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
                    let run_id = self.spawn_run(input).await;
                    write_response(&mut writer, &LocalResponse::Accepted { run_id }).await?;
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
            }
        }
        Ok(())
    }

    /// Starts a Run on its own task. The returned id is durable immediately;
    /// execution outlives the connection that asked for it.
    async fn spawn_run(self: &Arc<Self>, input: String) -> Uuid {
        let run_id = Uuid::now_v7();
        // Record before executing: a daemon that dies between these two points
        // must still leave evidence that this Run exists.
        let record = LocalRunRecord {
            store_version: crate::LOCAL_STORE_VERSION,
            tenant_id: self.invocation.tenant_id,
            application_id: self.invocation.application_id,
            workload_identity_id: self.invocation.workload_identity_id,
            workspace_id: self.invocation.workspace_id,
            agent_version_id: self.invocation.agent_version_id,
            model_policy_id: self.invocation.model_policy_id,
            run_id,
            input: input.clone(),
            state: LocalRunState::Running,
            owner_epoch: 1,
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &record).is_err() {
            return run_id;
        }
        self.launch(record, None, None, false).await;
        run_id
    }

    /// Answers the approval a parked Run is waiting on and lets it continue.
    /// Only a parked Run can be decided: answering anything else would either
    /// be a no-op or would restart work that is already in flight.
    async fn decide(
        self: &Arc<Self>,
        run_id: Uuid,
        decision: LocalApprovalDecision,
    ) -> LocalResponse {
        let Ok(Some(record)) = self.read_owned_record(run_id) else {
            return LocalResponse::Error {
                message: "unknown run".into(),
            };
        };
        let resolution = match &record.state {
            LocalRunState::AwaitingApproval {
                approval_id,
                binding_digest,
                target_run_id,
            } => LocalApprovalResolution {
                target_run_id: target_run_id.unwrap_or(run_id),
                approval_id: Some(*approval_id),
                binding_digest: Some(binding_digest.clone()),
                decision,
            },
            LocalRunState::ApprovalDecided {
                decision: recorded, ..
            } if *recorded == decision => return LocalResponse::Accepted { run_id },
            LocalRunState::ApprovalDecided { .. } => {
                return LocalResponse::Error {
                    message: "approval was already decided differently".into(),
                };
            }
            _ => {
                return LocalResponse::Error {
                    message: format!("run is not awaiting approval: {:?}", record.state),
                };
            }
        };
        let epoch = record.owner_epoch + 1;
        let resuming = LocalRunRecord {
            owner_epoch: epoch,
            state: LocalRunState::ApprovalDecided {
                target_run_id: resolution.target_run_id,
                approval_id: resolution.approval_id.expect("bound daemon approval id"),
                binding_digest: resolution
                    .binding_digest
                    .clone()
                    .expect("bound daemon approval digest"),
                decision,
            },
            ..record
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &resuming).is_err() {
            return LocalResponse::Error {
                message: "could not record the decision".into(),
            };
        }
        self.launch(
            resuming,
            Some(epoch),
            Some(LocalResumeResolution::Approval(resolution)),
            false,
        )
        .await;
        LocalResponse::Accepted { run_id }
    }

    async fn resolve_mcp_input(
        self: &Arc<Self>,
        resolution: LocalMcpInputResolution,
        run_id: Uuid,
    ) -> LocalResponse {
        let Ok(Some(record)) = self.read_owned_record(run_id) else {
            return LocalResponse::Error {
                message: "unknown run".into(),
            };
        };
        match &record.state {
            LocalRunState::AwaitingMcpInput { input }
                if input.input_id == resolution.input_id
                    && input.binding_digest == resolution.binding_digest => {}
            LocalRunState::McpInputDecided {
                resolution: recorded,
            } if recorded == &resolution => return LocalResponse::Accepted { run_id },
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
        let epoch = record.owner_epoch + 1;
        let resuming = LocalRunRecord {
            owner_epoch: epoch,
            state: LocalRunState::McpInputDecided {
                resolution: resolution.clone(),
            },
            ..record
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &resuming).is_err() {
            return LocalResponse::Error {
                message: "could not record the MCP input response".into(),
            };
        }
        self.launch(
            resuming,
            Some(epoch),
            Some(LocalResumeResolution::McpInput(resolution)),
            false,
        )
        .await;
        LocalResponse::Accepted { run_id }
    }

    /// Cancels a parked or active Run. Active execution owns the durable
    /// terminal event; this method only signals its downward cancellation tree.
    async fn cancel(self: &Arc<Self>, run_id: Uuid) -> LocalResponse {
        let Ok(Some(record)) = self.read_owned_record(run_id) else {
            return LocalResponse::Error {
                message: "unknown run".into(),
            };
        };
        if matches!(record.state, LocalRunState::Cancelling { .. }) {
            return LocalResponse::Accepted { run_id };
        }
        if record.state == LocalRunState::Running {
            let handle = self.runs.lock().await.get(&run_id).map(Arc::clone);
            let Some(handle) = handle else {
                return LocalResponse::Error {
                    message: "active run is not owned by this daemon".into(),
                };
            };
            let mut lifecycle = handle.lifecycle.lock().await;
            match &*lifecycle {
                RunLifecycle::Cancelling => return LocalResponse::Accepted { run_id },
                RunLifecycle::Finished(_) => {
                    return LocalResponse::Error {
                        message: "run is not cancellable".into(),
                    };
                }
                RunLifecycle::Running => {}
            }
            let Ok(Some(current)) = self.read_owned_record(run_id) else {
                return LocalResponse::Error {
                    message: "could not revalidate the active run".into(),
                };
            };
            if matches!(current.state, LocalRunState::Cancelling { .. }) {
                *lifecycle = RunLifecycle::Cancelling;
                return LocalResponse::Accepted { run_id };
            }
            if current.state != LocalRunState::Running {
                return LocalResponse::Error {
                    message: "run is not cancellable".into(),
                };
            }
            let cancelling = LocalRunRecord {
                state: LocalRunState::Cancelling {
                    reason: "cancelled by the local operator".into(),
                },
                ..current
            };
            if LocalRuntimeHost::write_run_record(&self.config.state_root, &cancelling).is_err() {
                return LocalResponse::Error {
                    message: "could not record the cancellation intent".into(),
                };
            }
            *lifecycle = RunLifecycle::Cancelling;
            handle.cancellation.cancel();
            return LocalResponse::Accepted { run_id };
        }
        if !matches!(
            record.state,
            LocalRunState::AwaitingApproval { .. } | LocalRunState::AwaitingMcpInput { .. }
        ) {
            return LocalResponse::Error {
                message: "run is not cancellable".into(),
            };
        }
        let cancelled = LocalRunRecord {
            state: LocalRunState::Cancelled {
                reason: "cancelled by the local operator while awaiting approval".into(),
            },
            ..record
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &cancelled).is_err() {
            return LocalResponse::Error {
                message: "could not record the cancellation".into(),
            };
        }
        if let Some(handle) = self.runs.lock().await.get(&run_id) {
            *handle.lifecycle.lock().await = RunLifecycle::Finished("cancelled".into());
        }
        LocalResponse::Accepted { run_id }
    }

    /// Picks up Runs an earlier daemon left unfinished. A Run without a
    /// Checkpoint is not restarted: nothing proves what it already did, and
    /// re-running is only safe when the Checkpoint says where to continue.
    pub async fn recover_unfinished(self: &Arc<Self>) -> Result<usize, LocalRuntimeError> {
        let records = LocalRuntimeHost::list_run_records(&self.config.state_root)?;
        let mut resumed = 0;
        for record in records {
            if !self.record_is_owned(&record) {
                continue;
            }
            let (cancellation_reason, resolution) = match &record.state {
                LocalRunState::Running => (None, None),
                LocalRunState::Cancelling { reason } => (Some(reason.clone()), None),
                LocalRunState::ApprovalDecided {
                    target_run_id,
                    approval_id,
                    binding_digest,
                    decision,
                } => (
                    None,
                    Some(LocalResumeResolution::Approval(LocalApprovalResolution {
                        target_run_id: *target_run_id,
                        approval_id: Some(*approval_id),
                        binding_digest: Some(binding_digest.clone()),
                        decision: *decision,
                    })),
                ),
                LocalRunState::McpInputDecided { resolution } => (
                    None,
                    Some(LocalResumeResolution::McpInput(resolution.clone())),
                ),
                _ => continue,
            };
            if self.runs.lock().await.contains_key(&record.run_id) {
                continue;
            }
            if let Some(terminal) =
                Self::terminal_state_from_events(&self.config.state_root, record.run_id)?
            {
                LocalRuntimeHost::write_run_record(
                    &self.config.state_root,
                    &LocalRunRecord {
                        state: terminal,
                        ..record
                    },
                )?;
                continue;
            }
            if !LocalRuntimeHost::checkpoint_path(&self.config.state_root, record.run_id).is_file()
            {
                let terminal = LocalRunRecord {
                    state: match cancellation_reason {
                        Some(reason) => LocalRunState::Cancelled { reason },
                        None => LocalRunState::Interrupted {
                            reason: "daemon stopped before the run produced a checkpoint".into(),
                        },
                    },
                    ..record
                };
                LocalRuntimeHost::write_run_record(&self.config.state_root, &terminal)?;
                continue;
            }
            // Recovery must outrank the epoch the Checkpoint bound, or restore
            // refuses it as a stale lease.
            let epoch = record.owner_epoch + 1;
            let resuming = LocalRunRecord {
                owner_epoch: epoch,
                ..record
            };
            LocalRuntimeHost::write_run_record(&self.config.state_root, &resuming)?;
            self.launch(
                resuming,
                Some(epoch),
                resolution,
                cancellation_reason.is_some(),
            )
            .await;
            resumed += 1;
        }
        Ok(resumed)
    }

    /// Reconciles the crash window after a terminal event append but before the
    /// daemon updates `run.json`. The event is the Kernel authority and must win
    /// over an older local lifecycle hint.
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

    /// Runs `record` on its own task, fresh when `resume_epoch` is `None` and
    /// from its Checkpoint otherwise, then durably records the outcome.
    async fn launch(
        self: &Arc<Self>,
        record: LocalRunRecord,
        resume_epoch: Option<u64>,
        resolution: Option<LocalResumeResolution>,
        cancel_on_start: bool,
    ) {
        let (sender, _) = broadcast::channel(LIVE_TAIL_CAPACITY);
        let lifecycle = Arc::new(Mutex::new(RunLifecycle::Running));
        let cancellation = tokio_util::sync::CancellationToken::new();
        if cancel_on_start {
            cancellation.cancel();
        }
        let handle = Arc::new(RunHandle {
            live: sender.clone(),
            lifecycle: Arc::clone(&lifecycle),
            cancellation: cancellation.clone(),
        });
        let run_id = record.run_id;
        self.runs.lock().await.insert(run_id, Arc::clone(&handle));
        if !self.order.lock().await.contains(&run_id) {
            self.order.lock().await.push(run_id);
        }

        let config = self.config.clone();
        let invocation = self.invocation;
        let state_root = self.config.state_root.clone();
        tokio::spawn(async move {
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let fanout = tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    // No receivers is normal: every client may be detached.
                    let _ = sender.send(event);
                }
            });
            let status = match LocalRuntimeHost::start_for_invocation_with_cancellation(
                config,
                invocation,
                cancellation,
            ) {
                Ok(mut host) => {
                    host.set_event_sink(events_tx);
                    let outcome = match (resume_epoch, resolution) {
                        (Some(epoch), Some(LocalResumeResolution::Approval(resolution))) => {
                            host.resume_with_resolution(run_id, &record.input, epoch, resolution)
                                .await
                        }
                        (Some(epoch), Some(LocalResumeResolution::McpInput(resolution))) => {
                            host.resume_with_mcp_input(run_id, &record.input, epoch, resolution)
                                .await
                        }
                        (Some(epoch), None) => host.resume(run_id, &record.input, epoch).await,
                        _ => host.execute_as(run_id, &record.input).await,
                    };
                    host.shutdown().await;
                    match outcome {
                        // A Run parked on an approval is not finished. Recording
                        // it as finished would make recovery skip it and leave
                        // it permanently unapprovable.
                        Ok(outcome) => {
                            if let Some(approval) = outcome.pending_approval {
                                Err(LocalRunState::AwaitingApproval {
                                    approval_id: approval.approval_id,
                                    binding_digest: approval.binding_digest,
                                    target_run_id: Some(approval.target_run_id),
                                })
                            } else if let Some(input) = outcome.pending_mcp_input {
                                Err(LocalRunState::AwaitingMcpInput { input })
                            } else {
                                Ok(outcome.status.as_str().to_owned())
                            }
                        }
                        Err(error) => Ok(format!("failed: {error}")),
                    }
                }
                Err(error) => Ok(format!("failed: {error}")),
            };
            fanout.abort();
            let (next_state, lifecycle_status) = match status {
                Ok(status) if status == "cancelled" => (
                    LocalRunState::Cancelled {
                        reason: "cancelled by the local operator".into(),
                    },
                    Some(status),
                ),
                Ok(status) => (
                    LocalRunState::Finished {
                        status: status.clone(),
                    },
                    Some(status),
                ),
                Err(parked) => (parked, None),
            };
            let updated = LocalRunRecord {
                state: next_state,
                ..record
            };
            // Durable before the in-memory lifecycle flips, so a crash between
            // the two cannot lose the outcome. The same lock serializes this
            // write against cancellation acknowledgement, so an older
            // `Cancelling` write cannot overwrite a terminal record.
            let mut lifecycle = lifecycle.lock().await;
            let _ = LocalRuntimeHost::write_run_record(&state_root, &updated);
            *lifecycle = match lifecycle_status {
                Some(status) => RunLifecycle::Finished(status),
                None => RunLifecycle::Finished("awaiting_approval".into()),
            };
        });
    }

    /// Replays the durable log first, then follows the live tail. Replay before
    /// subscribe would drop events produced in between, so the live subscription
    /// is opened first and replayed events are de-duplicated by sequence.
    async fn stream_run(
        self: &Arc<Self>,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        run_id: Uuid,
        after_sequence: u64,
    ) -> std::io::Result<()> {
        if self.read_owned_record(run_id).ok().flatten().is_none() {
            write_response(
                writer,
                &LocalResponse::Error {
                    message: "unknown run".into(),
                },
            )
            .await?;
            return Ok(());
        }
        let handle = self.runs.lock().await.get(&run_id).map(Arc::clone);
        let mut live = handle.as_ref().map(|handle| handle.live.subscribe());

        let replayed =
            LocalRuntimeHost::replay_events(&self.config.state_root, run_id, after_sequence)
                .unwrap_or_default();
        let mut highest = after_sequence;
        for event in replayed {
            highest = highest.max(event.sequence);
            write_response(
                writer,
                &LocalResponse::Event {
                    event: Box::new(event),
                },
            )
            .await?;
        }

        let Some(handle) = handle else {
            write_response(
                writer,
                &LocalResponse::Error {
                    message: "unknown run".into(),
                },
            )
            .await?;
            return Ok(());
        };

        loop {
            if let Some(receiver) = live.as_mut() {
                match receiver.try_recv() {
                    Ok(event) => {
                        if event.sequence > highest {
                            highest = event.sequence;
                            write_response(
                                writer,
                                &LocalResponse::Event {
                                    event: Box::new(event),
                                },
                            )
                            .await?;
                        }
                        continue;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        // The client fell behind the bounded tail. Replay the
                        // gap from the durable log instead of skipping it.
                        let missed = LocalRuntimeHost::replay_events(
                            &self.config.state_root,
                            run_id,
                            highest,
                        )
                        .unwrap_or_default();
                        for event in missed {
                            highest = highest.max(event.sequence);
                            write_response(
                                writer,
                                &LocalResponse::Event {
                                    event: Box::new(event),
                                },
                            )
                            .await?;
                        }
                        continue;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => live = None,
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }
            }
            let lifecycle = handle.lifecycle.lock().await.clone();
            if let RunLifecycle::Finished(status) = lifecycle {
                // Drain anything durable that landed after the last live poll.
                let tail =
                    LocalRuntimeHost::replay_events(&self.config.state_root, run_id, highest)
                        .unwrap_or_default();
                for event in tail {
                    highest = highest.max(event.sequence);
                    write_response(
                        writer,
                        &LocalResponse::Event {
                            event: Box::new(event),
                        },
                    )
                    .await?;
                }
                write_response(writer, &LocalResponse::Finished { run_id, status }).await?;
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
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
