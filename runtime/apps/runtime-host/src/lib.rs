//! Standalone local execution host (ADR-0035).
//!
//! Runs an Agent with no Java control plane, no PostgreSQL, no NATS, and no
//! gRPC. It links the Worker execution core as a library so the Skill/Tool
//! security invariants are the same code in local and cloud mode, and it calls
//! the provider adapters in-process because there is no boundary to cross.

pub mod ipc;

use agent_kernel::ToolPlan;
use agent_model_gateway::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderAdapter, ProviderCredential,
    ProviderPricing, decode_model_invocation,
};
use agent_protocol::{
    ApprovalMode, AutoApproval, EventEnvelope, ModelStreamEvent, RunBudget, RunExecutionCommand,
    RunStatus, SandboxClass, TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision,
    ToolApprovalDecisionCommand, ToolDescriptor, ToolEffect,
};
use agent_runtime_worker::{WorkerProcessor, WorkerToolDefinition};
use agent_tool_runtime::{
    ToolExecutionContext, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The trusted read-only workspace Tool, the only Tool a local host installs.
pub const WORKSPACE_READ_TOOL: &str = "workspace.read_text";
/// Scope the workspace Tool requires; a local Run that does not delegate it
/// cannot see the Tool, exactly as in cloud mode.
pub const WORKSPACE_READ_SCOPE: &str = "tool:workspace.read";
/// Write-capable counterpart. Contained by Seatbelt to the Workspace (ADR-0036)
/// and approval gated, because it changes the user's files.
pub const WORKSPACE_WRITE_TOOL: &str = "workspace.write_text";
pub const WORKSPACE_WRITE_SCOPE: &str = "tool:workspace.write";
/// Shell, same Tool and same containment as the cloud Worker installs. A local
/// host that offered fewer Tools than the cloud one would make the desktop
/// client a weaker product for no security reason -- the boundary is the
/// container, and it is the same container.
pub const SHELL_TOOL: &str = "shell.exec";
pub const SHELL_SCOPE: &str = "tool:shell.exec";

pub(crate) const LOCAL_STORE_VERSION: u32 = 1;

/// A local host has exactly one configured provider, so its model policy
/// identity is a fixed local constant. It must be stable across restarts:
/// recovery compares it against the identity the Checkpoint bound, and a fresh
/// value per process would make every local Run unresumable.
const LOCAL_MODEL_POLICY_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);

/// Fixed identities for the single local tenancy. The nil UUID is the "absent"
/// sentinel, so using it as a real identity both reads as missing data and is
/// rejected outright by contracts that require a complete identity.
const LOCAL_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0010);
const LOCAL_WORKSPACE_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0011);
const LOCAL_AGENT_VERSION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0012);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LocalRuntimeError {
    #[error("local runtime configuration is invalid: {0}")]
    Configuration(String),
    #[error("local state root is not usable: {0}")]
    StateRoot(String),
    /// Distinct from StateRoot on purpose: a client that sees this should
    /// connect to the running host, not report a broken installation.
    #[error("another runtime host is already serving this state root at {0}")]
    AlreadyRunning(String),
    #[error("local execution was refused: {0}")]
    Execution(String),
    #[error("model provider call failed: {0}")]
    Provider(String),
    #[error("trusted tool execution failed: {0}")]
    ToolExecution(String),
    #[error("local checkpoint is unusable: {0}")]
    Checkpoint(String),
}

/// The operator's answer to a parked approval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalApprovalDecision {
    AllowOnce,
    Deny,
}

/// How the local operator's consent reaches an approval-gated Tool. The gate is
/// never removed; `AllowOnce` only supplies the decision the cloud console
/// would supply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalToolConsent {
    Ask,
    AllowOnce,
}

#[derive(Clone, Debug)]
pub struct LocalProviderConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Clone, Debug)]
pub struct LocalRuntimeConfig {
    pub state_root: PathBuf,
    pub workspace_root: PathBuf,
    pub agent_instructions: String,
    pub delegated_scopes: BTreeSet<String>,
    pub provider: LocalProviderConfig,
    /// Absolute path to the trusted workspace Tool binary. Without it the host
    /// installs no Tools at all rather than falling back to anything untrusted.
    pub trusted_workspace_tool: Option<PathBuf>,
    pub consent: LocalToolConsent,
    pub budget: RunBudget,
}

/// Durable lifecycle of a local Run. Without it a restarted daemon cannot tell
/// a Run that is still owed work from one that already finished, and would
/// either re-execute completed Runs or abandon live ones.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalRunState {
    Running,
    /// Parked on the approval gate. Deliberately not `Finished`: a Run waiting
    /// for a human is still owed work, and recording it as finished would make
    /// recovery skip it forever and leave it permanently unapprovable.
    AwaitingApproval {
        approval_id: Uuid,
        binding_digest: String,
    },
    Finished {
        status: String,
    },
    Cancelled {
        reason: String,
    },
    /// The daemon died before the Run produced a Checkpoint, so there is
    /// nothing to resume from and re-running is not automatically safe.
    Interrupted {
        reason: String,
    },
}

/// The durable record a restarted daemon reads to decide what to do.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalRunRecord {
    pub store_version: u32,
    pub run_id: Uuid,
    pub input: String,
    pub state: LocalRunState,
    /// Highest owner epoch used so far. Recovery must exceed it, otherwise the
    /// Checkpoint is refused as a stale lease.
    pub owner_epoch: u64,
}

/// The approval a parked Run is waiting on. Both fields are needed to answer
/// it: the id names the decision, the binding digest proves the decision
/// applies to the exact Tool call that was planned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalPendingApproval {
    pub approval_id: Uuid,
    pub binding_digest: String,
}

/// One durable Run event as local clients see it. Persisted to the Run's event
/// log before it is broadcast, so a client that reconnects can replay exactly
/// what a client that stayed connected already received.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalEvent {
    pub sequence: u64,
    pub run_id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalRunOutcome {
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub status: RunStatus,
    pub event_types: Vec<String>,
    pub output: String,
    pub checkpoint_path: PathBuf,
    /// Set when execution stopped on an approval the operator has not answered.
    pub pending_approval: Option<LocalPendingApproval>,
}

pub struct LocalRuntimeHost {
    config: LocalRuntimeConfig,
    processor: WorkerProcessor,
    adapter: ProviderAdapter,
    credential: ProviderCredential,
    executors: std::collections::HashMap<String, TrustedNativeExecutor>,
    worker_id: Uuid,
    event_sink: Option<tokio::sync::mpsc::UnboundedSender<LocalEvent>>,
}

impl LocalRuntimeHost {
    pub fn start(config: LocalRuntimeConfig) -> Result<Self, LocalRuntimeError> {
        if config.agent_instructions.trim().is_empty() {
            return Err(LocalRuntimeError::Configuration(
                "agent instructions must not be blank".into(),
            ));
        }
        if !config.workspace_root.is_absolute() || !config.workspace_root.is_dir() {
            return Err(LocalRuntimeError::Configuration(
                "workspace root must be an existing absolute directory".into(),
            ));
        }
        // A host that cannot checkpoint cannot resume, and would lose work on
        // exit without ever saying so.
        std::fs::create_dir_all(config.state_root.join("runs"))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;

        let credential = ProviderCredential::bearer(config.provider.api_key.clone())
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
        let adapter = ProviderAdapter::from(
            OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
                endpoint: config.provider.endpoint.clone(),
                model: config.provider.model.clone(),
                // Local execution meters nothing, so pricing is zero rather
                // than a guess that would show up as a fabricated cost.
                pricing: ProviderPricing {
                    input_million_tokens_micros: 0,
                    output_million_tokens_micros: 0,
                },
                response_timeout: Duration::from_secs(120),
                stream_idle_timeout: Duration::from_secs(60),
            })
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?,
        );

        let worker_id = Uuid::now_v7();
        let mut processor = WorkerProcessor::new(
            worker_id,
            vec![agent_protocol::Placement::Edge],
            1,
            env!("CARGO_PKG_VERSION").to_string(),
        )
        .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;

        let mut executors = std::collections::HashMap::new();
        if let Some(binary) = &config.trusted_workspace_tool {
            let trusted_root = binary.parent().ok_or_else(|| {
                LocalRuntimeError::Configuration(
                    "trusted workspace tool must have a parent directory".into(),
                )
            })?;
            // One executor per Tool rather than one shared read-write executor:
            // the read Tool then runs under a profile that grants no writes at
            // all, so a defect in it cannot change anything.
            for (name, access, effect, scope, auto_approval, description) in [
                (
                    WORKSPACE_READ_TOOL,
                    WorkspaceAccess::ReadOnly,
                    ToolEffect::Pure,
                    WORKSPACE_READ_SCOPE,
                    AutoApproval::Never,
                    "Read one bounded UTF-8 text file from the local workspace",
                ),
                (
                    WORKSPACE_WRITE_TOOL,
                    WorkspaceAccess::ReadWrite,
                    ToolEffect::NonIdempotent,
                    WORKSPACE_WRITE_SCOPE,
                    AutoApproval::Never,
                    "Write one bounded UTF-8 text file into the local workspace",
                ),
                (
                    SHELL_TOOL,
                    WorkspaceAccess::ReadWrite,
                    ToolEffect::NonIdempotent,
                    SHELL_SCOPE,
                    AutoApproval::ProvablyReadOnlyShellCommand,
                    "Run one bounded shell command inside the local workspace",
                ),
            ] {
                let native = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
                    trusted_root: trusted_root.to_path_buf(),
                    executable: binary.clone(),
                    fixed_args: vec!["--stdio".into()],
                    workspace_access: access,
                    max_stdout_bytes: 128 * 1024,
                    max_stderr_bytes: 16 * 1024,
                })
                .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
                processor
                    .register_tool(WorkerToolDefinition {
                        descriptor: ToolDescriptor {
                            name: name.into(),
                            effect,
                            approval: ApprovalMode::Ask,
                            sandbox: SandboxClass::TrustedNative,
                            implementation_digest: native.implementation_digest().to_owned(),
                            required_scopes: BTreeSet::from([scope.to_owned()]),
                            auto_approval,
                        },
                        description: description.into(),
                        // Keyed on the Tool, not on its Workspace access: shell
                        // is also ReadWrite but takes a command, and matching on
                        // access would have handed it the file schema.
                        input_schema: match name {
                            WORKSPACE_READ_TOOL => serde_json::json!({
                                "type": "object",
                                "properties": {"path": {"type": "string"}},
                                "required": ["path"],
                                "additionalProperties": false
                            }),
                            SHELL_TOOL => serde_json::json!({
                                "type": "object",
                                "properties": {"command": {"type": "string"}},
                                "required": ["command"],
                                "additionalProperties": false
                            }),
                            _ => serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "text": {"type": "string"}
                                },
                                "required": ["path", "text"],
                                "additionalProperties": false
                            }),
                        },
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
                executors.insert(name.to_owned(), native);
            }
        }

        Ok(Self {
            config,
            processor,
            adapter,
            credential,
            executors,
            worker_id,
            event_sink: None,
        })
    }

    /// Builds the same `RunExecutionCommand` contract the Java scheduler emits.
    /// Owner epoch, fencing token, and incarnation exist to arbitrate between
    /// competing Workers; single-writer local execution has nothing to
    /// arbitrate, so they take fixed local values.
    fn local_command(&self, run_id: Uuid, input: &str, owner_epoch: u64) -> RunExecutionCommand {
        let issued_at = Utc::now();
        RunExecutionCommand {
            schema_version: 3,
            message_id: Uuid::now_v7(),
            tenant_id: LOCAL_TENANT_ID,
            run_id,
            session_id: run_id,
            workspace_id: LOCAL_WORKSPACE_ID,
            agent_version_id: LOCAL_AGENT_VERSION_ID,
            model_policy_id: LOCAL_MODEL_POLICY_ID,
            attempt_id: Uuid::now_v7(),
            worker_id: self.worker_id,
            worker_incarnation_id: self.worker_id,
            owner_epoch,
            fencing_token: Uuid::now_v7(),
            issued_at,
            lease_expires_at: issued_at + ChronoDuration::seconds(3600),
            // Local mode crosses no process boundary to reach the provider, so
            // there is no identity to present; this placeholder is never sent.
            workload_token: serde_json::from_value(serde_json::json!("local.local.local"))
                .expect("local workload token placeholder is well formed"),
            delegated_scopes: self.config.delegated_scopes.clone(),
            agent_instructions: self.config.agent_instructions.clone(),
            model_policy_snapshot_base64: String::new(),
            model_policy_digest: String::new(),
            skill_snapshots: Vec::new(),
            lineage: agent_protocol::AgentLineage::default(),
            subagent_roles: Vec::new(),
            input: input.to_owned(),
            budget: self.config.budget.clone(),
        }
    }

    fn run_dir(&self, run_id: Uuid) -> PathBuf {
        self.config.state_root.join("runs").join(run_id.to_string())
    }

    fn persist_checkpoint(
        &self,
        run_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<PathBuf, LocalRuntimeError> {
        let snapshot = self
            .processor
            .checkpoint(attempt_id)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let dir = self.run_dir(run_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let path = Self::checkpoint_path(&self.config.state_root, run_id);
        let body = serde_json::json!({
            "store_version": LOCAL_STORE_VERSION,
            "run_id": run_id,
            "checkpoint": snapshot,
        });
        // Write then rename so a crash mid-write cannot leave a torn checkpoint
        // that later refuses to restore.
        let staging = dir.join("checkpoint.json.partial");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(&body)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?,
        )
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        std::fs::rename(&staging, &path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Ok(path)
    }

    pub fn load_checkpoint(
        path: &Path,
    ) -> Result<agent_protocol::CheckpointSnapshot, LocalRuntimeError> {
        let body = std::fs::read(path)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        if value
            .get("store_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(LOCAL_STORE_VERSION))
        {
            return Err(LocalRuntimeError::Checkpoint(
                "unsupported local store version".into(),
            ));
        }
        serde_json::from_value(value["checkpoint"].clone())
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))
    }

    pub fn set_event_sink(&mut self, sink: tokio::sync::mpsc::UnboundedSender<LocalEvent>) {
        self.event_sink = Some(sink);
    }

    fn event_log_path(&self, run_id: Uuid) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    /// Persists the event, then broadcasts it. The order matters: a client that
    /// reconnects replays from the log, so an event that was broadcast but not
    /// yet durable would be visible to a connected client and invisible to a
    /// reconnecting one.
    fn emit(
        &self,
        run_id: Uuid,
        envelope: &EventEnvelope,
        types: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        types.push(envelope.event_type.clone());
        let event = LocalEvent {
            sequence: envelope.sequence,
            run_id,
            event_type: envelope.event_type.clone(),
            payload: envelope.payload.clone(),
        };
        let dir = self.run_dir(run_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        line.push(b'\n');
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.event_log_path(run_id))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        file.write_all(&line)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if let Some(sink) = &self.event_sink {
            // A detached client is not an execution failure; the Run continues.
            let _ = sink.send(event);
        }
        Ok(())
    }

    /// Replays a Run's durable event log from `after_sequence` (exclusive).
    pub fn replay_events(
        state_root: &Path,
        run_id: Uuid,
        after_sequence: u64,
    ) -> Result<Vec<LocalEvent>, LocalRuntimeError> {
        let path = state_root
            .join("runs")
            .join(run_id.to_string())
            .join("events.jsonl");
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let event: LocalEvent = serde_json::from_str(line)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if event.sequence > after_sequence {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub async fn execute(&mut self, input: &str) -> Result<LocalRunOutcome, LocalRuntimeError> {
        self.execute_as(Uuid::now_v7(), input).await
    }

    /// Executes under a caller-supplied Run id so a daemon can hand the id to a
    /// client before the work starts, and so the client can attach to the event
    /// log immediately.
    pub async fn execute_as(
        &mut self,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let command = self.local_command(run_id, input, 1);
        self.drive(command, None, None).await
    }

    /// Resumes a Run from its local Checkpoint on a fresh attempt. Restore
    /// re-derives the effective instructions, Tool catalog, and Skill identity
    /// and refuses the Checkpoint when any of them changed.
    pub async fn resume(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command(run_id, input, owner_epoch);
        self.drive(command, Some(checkpoint), None).await
    }

    /// Resumes a parked Run and answers the approval it was waiting on. The
    /// pending approval survives in the Checkpoint, so the decision is applied
    /// to the restored attempt after rebinding it to that attempt.
    pub async fn resume_with_decision(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        decision: LocalApprovalDecision,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command(run_id, input, owner_epoch);
        self.drive(command, Some(checkpoint), Some(decision)).await
    }

    #[must_use]
    pub fn checkpoint_path(state_root: &Path, run_id: Uuid) -> PathBuf {
        state_root
            .join("runs")
            .join(run_id.to_string())
            .join("checkpoint.json")
    }

    fn record_path(state_root: &Path, run_id: Uuid) -> PathBuf {
        state_root
            .join("runs")
            .join(run_id.to_string())
            .join("run.json")
    }

    pub fn write_run_record(
        state_root: &Path,
        record: &LocalRunRecord,
    ) -> Result<(), LocalRuntimeError> {
        let path = Self::record_path(state_root, record.run_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
        let staging = path.with_extension("json.partial");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(record)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?,
        )
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        std::fs::rename(&staging, &path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))
    }

    pub fn read_run_record(
        state_root: &Path,
        run_id: Uuid,
    ) -> Result<Option<LocalRunRecord>, LocalRuntimeError> {
        let Ok(body) = std::fs::read(Self::record_path(state_root, run_id)) else {
            return Ok(None);
        };
        let record: LocalRunRecord = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if record.store_version != LOCAL_STORE_VERSION {
            return Err(LocalRuntimeError::StateRoot(
                "unsupported local store version".into(),
            ));
        }
        Ok(Some(record))
    }

    /// Every Run this state root knows about, oldest first by Run id.
    pub fn list_run_records(state_root: &Path) -> Result<Vec<LocalRunRecord>, LocalRuntimeError> {
        let Ok(entries) = std::fs::read_dir(state_root.join("runs")) else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            if let Some(record) = Self::read_run_record(state_root, run_id)? {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.run_id);
        Ok(records)
    }

    async fn drive(
        &mut self,
        command: RunExecutionCommand,
        checkpoint: Option<agent_protocol::CheckpointSnapshot>,
        decision: Option<LocalApprovalDecision>,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let run_id = command.run_id;
        let attempt_id = command.attempt_id;
        let now = Utc::now();
        let mut event_types = Vec::new();

        match checkpoint {
            Some(snapshot) => {
                let receipt = self
                    .processor
                    .restore(command.clone(), snapshot, now)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &receipt.event, &mut event_types)?;
                if let Some(decision) = decision {
                    self.answer_pending_approval(run_id, attempt_id, decision, &mut event_types)
                        .await?;
                }
            }
            None => {
                self.processor
                    .accept(command.clone(), now)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                let started = self
                    .processor
                    .start(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &started, &mut event_types)?;
            }
        }

        let mut output = String::new();
        let mut pending_approval = None;
        let mut checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;

        loop {
            let prepared = self
                .processor
                .prepare_model_invocation(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            let request = decode_model_invocation(&prepared.invocation)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;

            let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
            let cancellation = CancellationToken::new();
            let call = self
                .adapter
                .execute(&request, &self.credential, cancellation, sender);
            let collector = async {
                let mut events = Vec::new();
                while let Some(event) = receiver.recv().await {
                    events.push(event);
                }
                events
            };
            let (result, events) = tokio::join!(call, collector);
            result.map_err(|error| LocalRuntimeError::Provider(format!("{error:?}")))?;

            for event in events {
                if let ModelStreamEvent::TextDelta { text } = &event {
                    output.push_str(text);
                }
                let envelope = self
                    .processor
                    .apply_model_event(attempt_id, event)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &envelope, &mut event_types)?;
            }

            if self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                break;
            }
            // Durable before Tool planning so a crash here recovers into
            // "tool calls pending" rather than replaying the model turn.
            self.persist_checkpoint(run_id, attempt_id)?;

            let awaiting = self
                .drain_tool_calls(run_id, attempt_id, &mut event_types)
                .await?;
            if let Some(approval) = awaiting {
                // Checkpoint before parking: the pending approval only becomes
                // answerable if it is durable, otherwise a restored attempt has
                // nothing to rebind the operator's decision onto.
                checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
                pending_approval = Some(approval);
                break;
            }
            checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
        }

        let status = self
            .processor
            .status(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        Ok(LocalRunOutcome {
            run_id,
            attempt_id,
            status,
            event_types,
            output,
            checkpoint_path,
            pending_approval,
        })
    }

    /// Rebinds the Checkpoint's pending approval onto the restored attempt and
    /// applies the operator's decision. Rebinding first is required: the
    /// approval was issued against the attempt that has since been replaced.
    async fn answer_pending_approval(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        decision: LocalApprovalDecision,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let rebound = self
            .processor
            .rebind_recovered_approval(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let approval_id = rebound
            .payload
            .get("approval")
            .and_then(|approval| approval.get("approval_id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                LocalRuntimeError::Execution("rebound approval has no approval id".into())
            })?;
        let binding_digest = rebound
            .payload
            .get("approval")
            .and_then(|approval| approval.get("execution"))
            .and_then(|execution| execution.get("binding_digest"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LocalRuntimeError::Execution("rebound approval has no binding digest".into())
            })?
            .to_owned();
        self.emit(run_id, &rebound, emitted)?;

        let issued_at = Utc::now();
        let outcome = self
            .processor
            .apply_tool_approval(
                ToolApprovalDecisionCommand {
                    schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                    message_id: Uuid::now_v7(),
                    tenant_id: LOCAL_TENANT_ID,
                    run_id,
                    attempt_id,
                    worker_id: self.worker_id,
                    worker_incarnation_id: self.worker_id,
                    approval_id,
                    approval_version: 2,
                    binding_digest,
                    decision: match decision {
                        LocalApprovalDecision::AllowOnce => ToolApprovalDecision::AllowOnce,
                        LocalApprovalDecision::Deny => ToolApprovalDecision::Deny,
                    },
                    issued_at,
                    expires_at: issued_at + ChronoDuration::minutes(5),
                },
                issued_at,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        for event in &outcome.events {
            self.emit(run_id, event, emitted)?;
        }
        if let Some(request) = outcome.execution {
            self.run_approved_tool(run_id, attempt_id, request, emitted)
                .await?;
        }
        Ok(())
    }

    /// Plans and runs every Tool call the model produced. Returns the emitted
    /// event types and, when consent is `Ask`, the approval that stopped it.
    async fn drain_tool_calls(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        emitted: &mut Vec<String>,
    ) -> Result<Option<LocalPendingApproval>, LocalRuntimeError> {
        loop {
            let planned = match self.processor.plan_next_tool_call(attempt_id) {
                Ok(planned) => planned,
                Err(agent_runtime_worker::WorkerAssignmentError::NoPendingToolCall) => break,
                Err(error) => return Err(LocalRuntimeError::Execution(error.to_string())),
            };
            self.emit(run_id, &planned.event, emitted)?;
            if let Some(followup) = &planned.followup_event {
                self.emit(run_id, followup, emitted)?;
            }
            let execution = match planned.plan {
                ToolPlan::Execute(request) => Some(request),
                ToolPlan::ApprovalRequired(approval) => {
                    if self.config.consent == LocalToolConsent::Ask {
                        return Ok(Some(LocalPendingApproval {
                            approval_id: approval.approval_id,
                            binding_digest: approval.execution.binding_digest.clone(),
                        }));
                    }
                    let issued_at = Utc::now();
                    let outcome = self
                        .processor
                        .apply_tool_approval(
                            ToolApprovalDecisionCommand {
                                schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                                message_id: Uuid::now_v7(),
                                tenant_id: LOCAL_TENANT_ID,
                                run_id,
                                attempt_id,
                                worker_id: self.worker_id,
                                worker_incarnation_id: self.worker_id,
                                approval_id: approval.approval_id,
                                approval_version: 2,
                                binding_digest: approval.execution.binding_digest.clone(),
                                decision: ToolApprovalDecision::AllowOnce,
                                issued_at,
                                expires_at: issued_at + ChronoDuration::minutes(5),
                            },
                            issued_at,
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    for event in &outcome.events {
                        self.emit(run_id, event, emitted)?;
                    }
                    outcome.execution
                }
                ToolPlan::Denied(_) => None,
                ToolPlan::SubagentSpawn(_) => {
                    return Err(LocalRuntimeError::Execution(
                        "local host does not run subagents yet".into(),
                    ));
                }
            };
            let Some(request) = execution else {
                continue;
            };
            self.run_approved_tool(run_id, attempt_id, request, emitted)
                .await?;
        }
        Ok(None)
    }

    /// Executes one authorized Tool call and feeds its bound result back. Shared
    /// by the inline path and the approve-after-restart path so a Tool answered
    /// by a client runs exactly as one answered in-process.
    async fn run_approved_tool(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;

        let executor = self.executors.get(&request.call.name).ok_or_else(|| {
            LocalRuntimeError::ToolExecution(format!(
                "no trusted tool executor is installed for {}",
                request.call.name
            ))
        })?;
        let result = executor
            .execute(
                request.clone(),
                ToolExecutionContext {
                    tenant_id: LOCAL_TENANT_ID,
                    run_id,
                    attempt_id,
                    workspace_root: self.config.workspace_root.clone(),
                    timeout: Duration::from_secs(30),
                    cancellation: CancellationToken::new(),
                    requested_at: Utc::now(),
                },
            )
            .await
            .map_err(|error| LocalRuntimeError::ToolExecution(error.to_string()))?;
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id.clone(),
                &request.binding_digest,
                result.content,
                result.is_error,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }
}
