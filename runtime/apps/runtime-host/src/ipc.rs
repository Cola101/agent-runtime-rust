//! Long-running local host and its Unix-socket IPC (ADR-0035 decision 7).
//!
//! The client is not the Runtime. A Run is owned by the daemon and by the local
//! store, so disconnecting a client must never cancel work, and reconnecting
//! must reconstruct what happened from the durable event log rather than from
//! anything the client remembered.

use crate::{
    LocalApprovalDecision, LocalEvent, LocalRunRecord, LocalRunState, LocalRuntimeConfig,
    LocalRuntimeError, LocalRuntimeHost,
};
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
    /// Close a parked Run without running the Tool it was waiting on.
    Cancel {
        run_id: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalResponse {
    Accepted { run_id: Uuid },
    Event { event: LocalEvent },
    Finished { run_id: Uuid, status: String },
    Runs { run_ids: Vec<Uuid> },
    Error { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RunLifecycle {
    Running,
    Finished(String),
}

struct RunHandle {
    live: broadcast::Sender<LocalEvent>,
    lifecycle: Arc<Mutex<RunLifecycle>>,
}

pub struct LocalRuntimeDaemon {
    config: LocalRuntimeConfig,
    runs: Arc<Mutex<HashMap<Uuid, Arc<RunHandle>>>>,
    order: Arc<Mutex<Vec<Uuid>>>,
}

impl LocalRuntimeDaemon {
    #[must_use]
    pub fn new(config: LocalRuntimeConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            runs: Arc::new(Mutex::new(HashMap::new())),
            order: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Binds the control socket. The socket is created with owner-only
    /// permissions because whoever can talk to it can spend the provider
    /// credential this host holds.
    pub async fn bind(socket_path: &Path) -> Result<UnixListener, LocalRuntimeError> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
        // A stale socket from a crashed host would otherwise make bind fail.
        if socket_path.exists() {
            std::fs::remove_file(socket_path)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
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
            run_id,
            input: input.clone(),
            state: LocalRunState::Running,
            owner_epoch: 1,
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &record).is_err() {
            return run_id;
        }
        self.launch(record, None, None).await;
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
        let Ok(Some(record)) = LocalRuntimeHost::read_run_record(&self.config.state_root, run_id)
        else {
            return LocalResponse::Error {
                message: "unknown run".into(),
            };
        };
        if !matches!(record.state, LocalRunState::AwaitingApproval { .. }) {
            return LocalResponse::Error {
                message: format!("run is not awaiting approval: {:?}", record.state),
            };
        }
        let epoch = record.owner_epoch + 1;
        let resuming = LocalRunRecord {
            owner_epoch: epoch,
            state: LocalRunState::Running,
            ..record
        };
        if LocalRuntimeHost::write_run_record(&self.config.state_root, &resuming).is_err() {
            return LocalResponse::Error {
                message: "could not record the decision".into(),
            };
        }
        self.launch(resuming, Some(epoch), Some(decision)).await;
        LocalResponse::Accepted { run_id }
    }

    /// Closes a parked Run without running the Tool it was waiting on.
    /// Cancelling a Run that is actively executing is not supported yet.
    async fn cancel(self: &Arc<Self>, run_id: Uuid) -> LocalResponse {
        let Ok(Some(record)) = LocalRuntimeHost::read_run_record(&self.config.state_root, run_id)
        else {
            return LocalResponse::Error {
                message: "unknown run".into(),
            };
        };
        if !matches!(record.state, LocalRunState::AwaitingApproval { .. }) {
            return LocalResponse::Error {
                message: "only a run awaiting approval can be cancelled".into(),
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
            if record.state != LocalRunState::Running {
                continue;
            }
            if self.runs.lock().await.contains_key(&record.run_id) {
                continue;
            }
            if !LocalRuntimeHost::checkpoint_path(&self.config.state_root, record.run_id).is_file()
            {
                let interrupted = LocalRunRecord {
                    state: LocalRunState::Interrupted {
                        reason: "daemon stopped before the run produced a checkpoint".into(),
                    },
                    ..record
                };
                LocalRuntimeHost::write_run_record(&self.config.state_root, &interrupted)?;
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
            self.launch(resuming, Some(epoch), None).await;
            resumed += 1;
        }
        Ok(resumed)
    }

    /// Runs `record` on its own task, fresh when `resume_epoch` is `None` and
    /// from its Checkpoint otherwise, then durably records the outcome.
    async fn launch(
        self: &Arc<Self>,
        record: LocalRunRecord,
        resume_epoch: Option<u64>,
        decision: Option<LocalApprovalDecision>,
    ) {
        let (sender, _) = broadcast::channel(LIVE_TAIL_CAPACITY);
        let lifecycle = Arc::new(Mutex::new(RunLifecycle::Running));
        let handle = Arc::new(RunHandle {
            live: sender.clone(),
            lifecycle: Arc::clone(&lifecycle),
        });
        let run_id = record.run_id;
        self.runs.lock().await.insert(run_id, Arc::clone(&handle));
        if !self.order.lock().await.contains(&run_id) {
            self.order.lock().await.push(run_id);
        }

        let config = self.config.clone();
        let state_root = self.config.state_root.clone();
        tokio::spawn(async move {
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
            let fanout = tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    // No receivers is normal: every client may be detached.
                    let _ = sender.send(event);
                }
            });
            let status = match LocalRuntimeHost::start(config) {
                Ok(mut host) => {
                    host.set_event_sink(events_tx);
                    let outcome = match (resume_epoch, decision) {
                        (Some(epoch), Some(decision)) => {
                            host.resume_with_decision(run_id, &record.input, epoch, decision)
                                .await
                        }
                        (Some(epoch), None) => host.resume(run_id, &record.input, epoch).await,
                        _ => host.execute_as(run_id, &record.input).await,
                    };
                    match outcome {
                        // A Run parked on an approval is not finished. Recording
                        // it as finished would make recovery skip it and leave
                        // it permanently unapprovable.
                        Ok(outcome) => match outcome.pending_approval {
                            Some(approval) => Err(LocalRunState::AwaitingApproval {
                                approval_id: approval.approval_id,
                                binding_digest: approval.binding_digest,
                            }),
                            None => Ok(format!("{:?}", outcome.status).to_lowercase()),
                        },
                        Err(error) => Ok(format!("failed: {error}")),
                    }
                }
                Err(error) => Ok(format!("failed: {error}")),
            };
            fanout.abort();
            let (next_state, lifecycle_status) = match status {
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
            // the two cannot lose the outcome.
            let _ = LocalRuntimeHost::write_run_record(&state_root, &updated);
            *lifecycle.lock().await = match lifecycle_status {
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
        let handle = self.runs.lock().await.get(&run_id).map(Arc::clone);
        let mut live = handle.as_ref().map(|handle| handle.live.subscribe());

        let replayed =
            LocalRuntimeHost::replay_events(&self.config.state_root, run_id, after_sequence)
                .unwrap_or_default();
        let mut highest = after_sequence;
        for event in replayed {
            highest = highest.max(event.sequence);
            write_response(writer, &LocalResponse::Event { event }).await?;
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
                            write_response(writer, &LocalResponse::Event { event }).await?;
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
                            write_response(writer, &LocalResponse::Event { event }).await?;
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
                    write_response(writer, &LocalResponse::Event { event }).await?;
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
