use agent_grpc_security::ClientMtlsMaterials;
use agent_kernel::{RunCommand, RunMachine, ToolPlan, ToolRegistry};
use agent_model_gateway_protocol::v1::content_part;
use agent_model_gateway_protocol::v1::{
    ContentPart, ModelInvocation, ModelMessage, ModelRole, ModelTool, ReasoningPolicy, TextPart,
    ToolCallPart, ToolResultPart,
};
use agent_nats_security::NatsClientConfig;
use agent_protocol::{
    ActiveRunAssignment, ApprovalMode, BudgetDimension, EventEnvelope, ModelFinishReason,
    ModelStreamEvent, Placement, PreparedRunCheckpoint, RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION,
    RunCancellationCommand, RunCheckpointPublished, RunExecutionAccepted, RunExecutionCommand,
    RunRecoveryCommand, RunSteeringCommand, RunSteeringOutcome, SandboxClass,
    SubagentResultDelivery, SubagentRole, SubagentSpawnRequest, ToolApprovalDecision,
    ToolApprovalDecisionCommand, ToolCall, ToolDescriptor, ToolEffect, ToolExecutionRequest,
    WORKER_HEARTBEAT_SCHEMA_VERSION, WorkerHeartbeat, WorkloadIdentityRenewalCommand,
    WorkloadToken,
};
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, ToolExecutor, TrustedNativeExecutor,
    TrustedNativeToolDefinition, WorkspaceAccess,
};
use agent_workload_identity::{RequiredCapability, WorkloadIdentityBinding, WorkloadTokenVerifier};
use base64::Engine;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::{StreamExt, future::BoxFuture};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod checkpoint_gateway;
mod execution_supervisor;
mod mcp_gateway;
mod model_gateway;
mod tool_execution_supervisor;

pub use checkpoint_gateway::GrpcCheckpointPayloadStore;
pub use execution_supervisor::{ModelExecutionSupervisor, ModelExecutionUpdate};
pub use mcp_gateway::{
    discover_federated_tools, DiscoveredCatalog, DiscoveredTool, FederatedRunTools,
    GrpcMcpFederationClient, McpGatewayClientError,
};
pub use model_gateway::{GrpcModelGatewayClient, ModelGatewayClientError};
pub use tool_execution_supervisor::{ToolExecutionSupervisor, ToolExecutionUpdate};

pub const EXECUTION_STREAM_NAME: &str = "RUNTIME_EXECUTION";
pub const WORKER_EVENT_STREAM_NAME: &str = "RUNTIME_WORKER";
pub const WORKER_HEARTBEAT_SUBJECT: &str = "runtime.worker.heartbeat.v2";
pub const EXECUTION_ACCEPTED_SUBJECT: &str = "runtime.worker.execution.accepted.v2";
pub const RUN_EVENT_SUBJECT: &str = "runtime.worker.run.event.v1";
pub const CHECKPOINT_SUBJECT: &str = "runtime.worker.run.checkpoint.v1";
pub const RUN_STEERING_OUTCOME_SUBJECT: &str = "runtime.worker.run.steering.outcome.v1";
pub const WORKER_CHECKPOINT_SCHEMA_VERSION: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum WorkerIdentityError {
    #[error("failed to access persisted worker identity at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("persisted worker identity at {path} is invalid: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: uuid::Error,
    },
}

/// Loads a stable worker identity from its persistent volume, creating it once.
/// A corrupt identity fails closed so a pod cannot silently impersonate a new worker.
pub fn load_or_create_worker_id(path: impl AsRef<Path>) -> Result<Uuid, WorkerIdentityError> {
    let path = path.as_ref();
    match fs::read_to_string(path) {
        Ok(value) => Uuid::parse_str(value.trim()).map_err(|source| WorkerIdentityError::Invalid {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| WorkerIdentityError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let worker_id = Uuid::now_v7();
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    file.write_all(worker_id.to_string().as_bytes())
                        .and_then(|_| file.sync_all())
                        .map_err(|source| WorkerIdentityError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    Ok(worker_id)
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    load_or_create_worker_id(path)
                }
                Err(source) => Err(WorkerIdentityError::Io {
                    path: path.to_path_buf(),
                    source,
                }),
            }
        }
        Err(source) => Err(WorkerIdentityError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CheckpointStoreError {
    #[error("checkpoint object is not available")]
    NotFound,
    #[error("checkpoint store is unavailable: {0}")]
    Unavailable(String),
    #[error("checkpoint object failed integrity verification")]
    Corrupt,
}

#[derive(Clone)]
pub struct CheckpointStoreContext {
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub workload_token: String,
}

pub trait CheckpointPayloadStore: Send + Sync {
    fn put<'a>(
        &'a self,
        context: &'a CheckpointStoreContext,
        payload_ref: &'a str,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<(), CheckpointStoreError>>;

    fn get<'a>(
        &'a self,
        context: &'a CheckpointStoreContext,
        payload_ref: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, CheckpointStoreError>>;
}

fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn budget_exhaustion(
    usage: BudgetUsage,
    budget: &agent_protocol::RunBudget,
) -> Option<PendingBudgetExhaustion> {
    let maximum_cost_micros = budget.max_cost_cents.saturating_mul(10_000);
    if usage.tokens > budget.max_tokens {
        return Some(PendingBudgetExhaustion {
            dimension: BudgetDimension::Tokens,
            exceeded: true,
        });
    }
    if usage.cost_micros > maximum_cost_micros {
        return Some(PendingBudgetExhaustion {
            dimension: BudgetDimension::Cost,
            exceeded: true,
        });
    }
    if usage.tokens == budget.max_tokens {
        return Some(PendingBudgetExhaustion {
            dimension: BudgetDimension::Tokens,
            exceeded: false,
        });
    }
    if usage.cost_micros == maximum_cost_micros {
        return Some(PendingBudgetExhaustion {
            dimension: BudgetDimension::Cost,
            exceeded: false,
        });
    }
    None
}

#[derive(Clone, Debug)]
pub struct SkillArtifactVerifier {
    signing_key_id: String,
    verifying_key: VerifyingKey,
}

impl SkillArtifactVerifier {
    #[must_use]
    pub fn new(signing_key_id: impl Into<String>, verifying_key: VerifyingKey) -> Self {
        Self {
            signing_key_id: signing_key_id.into(),
            verifying_key,
        }
    }

    pub fn from_base64(
        signing_key_id: impl Into<String>,
        encoded: &str,
    ) -> Result<Self, WorkerAssignmentError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| WorkerAssignmentError::SkillVerifierConfiguration)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| WorkerAssignmentError::SkillVerifierConfiguration)?;
        let verifying_key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| WorkerAssignmentError::SkillVerifierConfiguration)?;
        Ok(Self::new(signing_key_id, verifying_key))
    }

    fn verify(&self, snapshot: &agent_protocol::SkillSnapshot) -> bool {
        if snapshot.signing_key_id != self.signing_key_id {
            return false;
        }
        let Ok(bytes) =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&snapshot.signature)
        else {
            return false;
        };
        let Ok(signature) = Signature::from_slice(&bytes) else {
            return false;
        };
        self.verifying_key
            .verify_strict(
                format!("agent-runtime-skill-v1.{}", snapshot.artifact_digest).as_bytes(),
                &signature,
            )
            .is_ok()
    }
}

#[derive(Debug)]
struct EffectiveSkillState {
    agent_instructions: String,
    tool_names: BTreeSet<String>,
    tool_catalog_digest: String,
    /// The catalog digest as Workers computed it before Tool activation was
    /// narrowed by the delegated scopes. Checkpoints written by those releases
    /// (checkpoint schema below 5) recorded this value, so recovery has to
    /// compare against the rule that actually produced the stored digest.
    /// Delete with the next checkpoint schema bump, once no such checkpoint can
    /// still be in flight.
    legacy_tool_catalog_digest: String,
    skill_binding_digest: String,
}

/// Binds the immutable identity of the Skills that were actually loaded, in
/// their declared order. The rendered instructions and Tool catalog cannot do
/// this on their own: two distinct `SkillVersion`s may render byte-identical
/// instructions and declare the same Tools, so recovery would otherwise accept
/// a substituted Skill artifact as unchanged.
fn skill_binding_digest(snapshots: &[agent_protocol::SkillSnapshot]) -> String {
    let bindings = snapshots
        .iter()
        .map(|skill| {
            serde_json::json!({
                "artifact_digest": skill.artifact_digest,
                "signing_key_id": skill.signing_key_id,
                "skill_version_id": skill.skill_version_id,
            })
        })
        .collect::<Vec<_>>();
    digest_bytes(&serde_json::to_vec(&bindings).expect("skill binding is serializable"))
}

#[derive(Debug)]
pub struct WorkerProcessor {
    worker_id: Uuid,
    worker_incarnation_id: Uuid,
    placements: Vec<Placement>,
    capacity: u32,
    runtime_version: String,
    admission_fence: WorkerAdmissionFence,
    draining: Option<DrainState>,
    accepted: HashMap<Uuid, ActiveExecution>,
    completed: HashMap<Uuid, CompletionReceipt>,
    tool_registry: ToolRegistry,
    tool_definitions: BTreeMap<String, WorkerToolDefinition>,
    skill_artifact_verifier: Option<SkillArtifactVerifier>,
}

/// Process-wide, one-way admission fence. Signal handlers can close it without
/// cancelling a transport operation that may already be committing an event.
#[derive(Clone, Debug)]
pub struct WorkerAdmissionFence {
    open: Arc<AtomicBool>,
}

impl WorkerAdmissionFence {
    fn new() -> Self {
        Self {
            open: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DrainState {
    started_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

#[derive(Debug)]
struct ActiveExecution {
    command: RunExecutionCommand,
    identity_generation: u64,
    accepted: RunExecutionAccepted,
    machine: RunMachine,
    cancellation: CancellationToken,
    started_event: Option<EventEnvelope>,
    terminal_event: Option<EventEnvelope>,
    transcript: Vec<ModelMessage>,
    effective_agent_instructions: String,
    effective_tool_names: BTreeSet<String>,
    effective_tool_catalog_digest: String,
    effective_skill_binding_digest: String,
    pending_tool_calls: VecDeque<ToolCall>,
    outstanding_tool_calls: HashMap<String, ToolExecutionRequest>,
    started_tool_calls: HashMap<String, EventEnvelope>,
    recovery_replanned_tools: HashMap<String, EventEnvelope>,
    rebound_approval_event: Option<EventEnvelope>,
    pending_approval: Option<agent_protocol::ToolApprovalRequest>,
    pending_subagent: Option<SubagentSpawnRequest>,
    subagent_result_receipt: Option<(String, EventEnvelope)>,
    steering_receipts: HashMap<Uuid, SteeringReceipt>,
    budget_usage: BudgetUsage,
    pending_budget_exhaustion: Option<PendingBudgetExhaustion>,
    approval_decisions: HashMap<Uuid, ApprovalDecisionReceipt>,
    restored_from_checkpoint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SteeringReceipt {
    input_digest: String,
    event: EventEnvelope,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct BudgetUsage {
    tokens: u64,
    cost_micros: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingBudgetExhaustion {
    dimension: BudgetDimension,
    exceeded: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentSpawnArguments {
    role: String,
    input: String,
    max_tokens: u64,
    max_cost_cents: u64,
    max_duration_seconds: u64,
}

fn subagent_spawn_tool(roles: &[SubagentRole]) -> ModelTool {
    let role_names = roles
        .iter()
        .map(|role| role.name.as_str())
        .collect::<Vec<_>>();
    ModelTool {
        name: "agent.spawn".into(),
        description: "Delegate one bounded task to an authorized subagent role".into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "role": {"type": "string", "enum": role_names},
                "input": {"type": "string", "minLength": 1, "maxLength": 32000},
                "max_tokens": {"type": "integer", "minimum": 1},
                "max_cost_cents": {"type": "integer", "minimum": 1},
                "max_duration_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
            },
            "required": [
                "role", "input", "max_tokens", "max_cost_cents", "max_duration_seconds"
            ],
            "additionalProperties": false
        }))
        .expect("subagent tool schema is serializable"),
    }
}

fn deterministic_delegation_id(command: &RunExecutionCommand, tool_call_id: &str) -> Uuid {
    let material = format!(
        "agent-runtime-subagent-v1\n{}\n{}\n{}",
        command.tenant_id, command.run_id, tool_call_id
    );
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn subagent_binding_digest(
    command: &RunExecutionCommand,
    call: &ToolCall,
    delegation_id: Uuid,
    arguments: &SubagentSpawnArguments,
) -> String {
    let material = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "tenant_id": command.tenant_id,
        "parent_run_id": command.run_id,
        "parent_attempt_id": command.attempt_id,
        "tool_call_id": call.id,
        "delegation_id": delegation_id,
        "role": arguments.role,
        "input": arguments.input,
        "budget": {
            "max_tokens": arguments.max_tokens,
            "max_cost_cents": arguments.max_cost_cents,
            "max_duration_seconds": arguments.max_duration_seconds
        }
    }))
    .expect("subagent binding material is serializable");
    digest_bytes(&material)
}

#[derive(Debug)]
struct ApprovalDecisionReceipt {
    command: ToolApprovalDecisionCommand,
    outcome: ToolApprovalOutcome,
}

#[derive(Debug)]
struct CompletionReceipt {
    run_id: Uuid,
    terminal_event: EventEnvelope,
}

#[derive(Debug)]
pub struct PreparedModelInvocation {
    pub invocation: ModelInvocation,
    pub workload_token: WorkloadToken,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkerToolDefinition {
    pub descriptor: ToolDescriptor,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One trusted Tool with the executor that contains it. Each Tool gets its own
/// executor so a read-only Tool keeps running under a profile that grants no
/// writes at all (ADR-0036).
pub struct TrustedWorkspaceTool {
    pub definition: WorkerToolDefinition,
    pub executor: Arc<TrustedNativeExecutor>,
}

pub struct TrustedWorkspaceToolRegistration {
    pub tools: Vec<TrustedWorkspaceTool>,
    pub workspace_root: PathBuf,
}

pub fn prepare_trusted_workspace_tool(
    enabled: bool,
    executable: PathBuf,
    workspace_root: PathBuf,
) -> Result<Option<TrustedWorkspaceToolRegistration>, WorkerAssignmentError> {
    if !enabled {
        return Ok(None);
    }
    if !workspace_root.is_absolute() || !workspace_root.is_dir() {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "trusted native workspace root must be an existing absolute directory".into(),
        ));
    }
    let trusted_root = executable
        .parent()
        .ok_or_else(|| {
            WorkerAssignmentError::ToolExecutorConfiguration(
                "trusted native tool executable must have a parent directory".into(),
            )
        })?
        .to_path_buf();

    let mut tools = Vec::new();
    for (name, access, effect, scope, description, schema) in [
        (
            "workspace.read_text",
            WorkspaceAccess::ReadOnly,
            ToolEffect::Pure,
            "tool:workspace.read",
            "Read one bounded UTF-8 text file from the current workspace",
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        (
            "workspace.write_text",
            WorkspaceAccess::ReadWrite,
            // A write changes the user's files, so an ambiguous failure must
            // never be replayed automatically.
            ToolEffect::NonIdempotent,
            "tool:workspace.write",
            "Write one bounded UTF-8 text file into the current workspace",
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "text": {"type": "string"}},
                "required": ["path", "text"],
                "additionalProperties": false
            }),
        ),
        (
            "shell.exec",
            // A command can write, so the Workspace has to be writable; what
            // stops it reaching further is Seatbelt, not this flag.
            WorkspaceAccess::ReadWrite,
            // Arbitrary side effects. An ambiguous failure must never replay.
            ToolEffect::NonIdempotent,
            // Its own scope. Granting shell is a different decision from
            // granting file writes, and must not ride along with one.
            "tool:shell.exec",
            "Run one bounded shell command inside the current workspace",
            serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
    ] {
        let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
            trusted_root: trusted_root.clone(),
            executable: executable.clone(),
            fixed_args: vec!["--stdio".into()],
            workspace_access: access,
            max_stdout_bytes: 128 * 1024,
            max_stderr_bytes: 16 * 1024,
        })
        .map_err(|error| WorkerAssignmentError::ToolExecutorConfiguration(error.to_string()))?;
        let implementation_digest = executor.implementation_digest().to_owned();
        tools.push(TrustedWorkspaceTool {
            definition: WorkerToolDefinition {
                descriptor: ToolDescriptor {
                    name: name.into(),
                    effect,
                    approval: ApprovalMode::Ask,
                    sandbox: SandboxClass::TrustedNative,
                    implementation_digest,
                    required_scopes: BTreeSet::from([scope.to_string()]),
                },
                description: description.into(),
                input_schema: schema,
            },
            executor: Arc::new(executor),
        });
    }

    Ok(Some(TrustedWorkspaceToolRegistration {
        tools,
        workspace_root,
    }))
}

/// Turns a discovered MCP catalog into Tool definitions the kernel accepts.
///
/// This is the joint between federation and the kernel. Everything about a
/// federated Tool that matters for safety is decided here, in one place, rather
/// than being whatever the server happened to send:
///
/// - `SandboxClass::Federated`, which is what stops any approval exemption
///   reaching it (ADR-0040 decision 6, enforced in the kernel).
/// - `ApprovalMode::Ask` and `ToolEffect::Unknown`, because its effects are
///   unknown by construction -- that is what third-party means.
/// - the frozen catalog digest as the implementation digest, so a Checkpoint
///   restore recomputes it and refuses when the catalog moved.
/// - `tool:mcp:<server>` as the required scope, so a Skill still cannot reach a
///   server the AgentVersion never delegated.
pub fn federated_tool_definitions(
    server_name: &str,
    frozen_catalog_digest: &str,
    tools: impl IntoIterator<Item = (String, String, serde_json::Value)>,
) -> Result<Vec<WorkerToolDefinition>, WorkerAssignmentError> {
    if frozen_catalog_digest.len() != 64
        || !frozen_catalog_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkerAssignmentError::ToolConfiguration(
            "federated tools need the frozen catalog digest as their implementation digest".into(),
        ));
    }
    let prefix = format!("mcp:{server_name}/");
    let mut definitions = Vec::new();
    for (qualified_name, description, input_schema) in tools {
        // A catalog entry naming a tool outside this server's namespace would
        // register a Tool under a namespace nobody delegated. The gateway
        // already qualifies names, so reaching here means something upstream is
        // wrong and guessing would be worse than refusing.
        if !qualified_name.starts_with(&prefix) {
            return Err(WorkerAssignmentError::ToolConfiguration(format!(
                "federated tool {qualified_name} is not namespaced under {prefix}"
            )));
        }
        definitions.push(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: qualified_name,
                effect: ToolEffect::Unknown,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::Federated,
                implementation_digest: frozen_catalog_digest.to_owned(),
                required_scopes: BTreeSet::from([format!("tool:mcp:{server_name}")]),
            },
            description: if description.trim().is_empty() {
                // register_tool refuses a blank description, and a server is
                // entitled to omit one. Refusing the whole catalog over a
                // missing sentence would be the wrong failure.
                "Federated MCP tool".to_owned()
            } else {
                description
            },
            input_schema: if input_schema.is_object() {
                input_schema
            } else {
                serde_json::json!({ "type": "object" })
            },
        });
    }
    Ok(definitions)
}

pub fn materialize_native_workspace(
    workspace_base: &Path,
    tenant_id: Uuid,
    workspace_id: Uuid,
) -> Result<PathBuf, WorkerAssignmentError> {
    if !workspace_base.is_absolute() {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "workspace base must be an absolute path".into(),
        ));
    }
    let base_metadata = fs::symlink_metadata(workspace_base).map_err(|error| {
        WorkerAssignmentError::ToolExecutorConfiguration(format!(
            "workspace base cannot be inspected: {error}"
        ))
    })?;
    if base_metadata.file_type().is_symlink() || !base_metadata.is_dir() {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "workspace base must be a real directory".into(),
        ));
    }
    let canonical_base = fs::canonicalize(workspace_base).map_err(|error| {
        WorkerAssignmentError::ToolExecutorConfiguration(format!(
            "workspace base cannot be resolved: {error}"
        ))
    })?;
    let tenant_root = ensure_uuid_workspace_directory(&canonical_base, tenant_id)?;
    let workspace_root = ensure_uuid_workspace_directory(&tenant_root, workspace_id)?;
    if !workspace_root.starts_with(&canonical_base) {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "workspace escaped its configured base".into(),
        ));
    }
    Ok(workspace_root)
}

fn ensure_uuid_workspace_directory(
    parent: &Path,
    identity: Uuid,
) -> Result<PathBuf, WorkerAssignmentError> {
    let directory = parent.join(identity.to_string());
    match fs::create_dir(&directory) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        WorkerAssignmentError::ToolExecutorConfiguration(format!(
                            "workspace permissions cannot be restricted: {error}"
                        ))
                    },
                )?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "workspace directory cannot be created: {error}"
            )));
        }
    }
    let metadata = fs::symlink_metadata(&directory).map_err(|error| {
        WorkerAssignmentError::ToolExecutorConfiguration(format!(
            "workspace directory cannot be inspected: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "workspace path must be a real directory".into(),
        ));
    }
    let canonical = fs::canonicalize(&directory).map_err(|error| {
        WorkerAssignmentError::ToolExecutorConfiguration(format!(
            "workspace directory cannot be resolved: {error}"
        ))
    })?;
    if canonical.parent() != Some(parent) {
        return Err(WorkerAssignmentError::ToolExecutorConfiguration(
            "workspace directory crossed its UUID parent boundary".into(),
        ));
    }
    Ok(canonical)
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedToolCall {
    pub plan: ToolPlan,
    pub event: EventEnvelope,
    pub followup_event: Option<EventEnvelope>,
    pub subagent_request: Option<SubagentSpawnRequest>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolApprovalOutcome {
    pub events: Vec<EventEnvelope>,
    pub execution: Option<ToolExecutionRequest>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkerRestoreReceipt {
    pub accepted: RunExecutionAccepted,
    pub event: EventEnvelope,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerRecoveryAction {
    InvokeModel,
    PlanPendingTool,
    RetryTool(ToolExecutionRequest),
    WaitForApproval,
    WaitForSubagent,
    TerminateBudgetExceeded(BudgetDimension),
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkerCheckpointState {
    schema_version: u32,
    runtime_version: String,
    workspace_id: Uuid,
    agent_version_id: Uuid,
    model_policy_id: Uuid,
    input_digest: String,
    #[serde(default)]
    agent_instructions_digest: String,
    /// Written from schema 5 onwards; older checkpoints carry no Skill identity
    /// and are still restorable under their original rules.
    #[serde(default)]
    skill_binding_digest: String,
    #[serde(default)]
    lineage: agent_protocol::AgentLineage,
    #[serde(default)]
    subagent_roles: Vec<SubagentRole>,
    budget: agent_protocol::RunBudget,
    delegated_scopes: std::collections::BTreeSet<String>,
    owner_epoch: u64,
    fencing_token: Uuid,
    tool_catalog_digest: String,
    transcript: Vec<Vec<u8>>,
    pending_tool_calls: Vec<ToolCall>,
    outstanding_tool_calls: BTreeMap<String, ToolExecutionRequest>,
    started_tool_calls: BTreeMap<String, EventEnvelope>,
    pending_approval: Option<agent_protocol::ToolApprovalRequest>,
    #[serde(default)]
    pending_subagent: Option<SubagentSpawnRequest>,
    #[serde(default)]
    budget_usage: BudgetUsage,
    #[serde(default)]
    pending_budget_exhaustion: Option<PendingBudgetExhaustion>,
    #[serde(default)]
    steering_receipts: BTreeMap<Uuid, SteeringReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkerAssignmentError {
    #[error("worker configuration is invalid")]
    InvalidConfiguration,
    #[error("worker Skill verifier configuration is invalid")]
    SkillVerifierConfiguration,
    #[error("execution Skill signature or runtime compatibility is invalid")]
    InvalidSkillArtifact,
    #[error("execution command targets another worker")]
    WrongWorker,
    #[error("execution command targets another worker incarnation")]
    WrongWorkerIncarnation,
    #[error("execution command is invalid: {0}")]
    InvalidCommand(String),
    #[error("execution lease has expired")]
    LeaseExpired,
    #[error("workload identity renewal is invalid: {0}")]
    InvalidWorkloadIdentityRenewal(String),
    #[error("workload identity renewal does not match the active execution")]
    WorkloadIdentityBindingMismatch,
    #[error("workload identity renewal generation is stale")]
    StaleWorkloadIdentityRenewal,
    #[error("run cancellation command is invalid: {0}")]
    InvalidCancellation(String),
    #[error("run cancellation command has expired")]
    CancellationExpired,
    #[error("run steering command is invalid: {0}")]
    InvalidSteering(String),
    #[error("run steering command has expired")]
    SteeringExpired,
    #[error("run steering id was reused for different input")]
    SteeringConflict,
    #[error("run steering is unsafe while a tool, approval, or subagent operation is unresolved")]
    SteeringUnsafe,
    #[error("worker has no remaining capacity")]
    AtCapacity,
    #[error("worker is draining and cannot accept new attempts")]
    Draining,
    #[error("worker drain deadline must be after draining starts")]
    InvalidDrainWindow,
    #[error("worker drain state is already fenced")]
    AlreadyDraining,
    #[error("attempt id was reused for another run")]
    AttemptConflict,
    #[error("execution attempt is not active")]
    UnknownAttempt,
    #[error("execution attempt is already terminal")]
    AttemptAlreadyTerminal,
    #[error("execution attempt has no terminal event to acknowledge")]
    TerminalNotReady,
    #[error("terminal event acknowledgement does not match the active attempt")]
    TerminalEventMismatch,
    #[error("agent kernel rejected execution start: {0}")]
    KernelTransition(String),
    #[error("tool configuration is invalid: {0}")]
    ToolConfiguration(String),
    #[error("model returned a duplicate or malformed tool call")]
    InvalidToolCall,
    #[error("model completed a tool turn without a tool call")]
    EmptyToolTurn,
    #[error("there is no tool call ready to plan")]
    NoPendingToolCall,
    #[error("tool call is not awaiting a result")]
    ToolCallNotExecuting,
    #[error("tool result does not match the original execution request")]
    ToolResultBindingMismatch,
    #[error("tool execution start does not match the original execution request")]
    ToolExecutionBindingMismatch,
    #[error("tool result arrived before execution start was durably recorded")]
    ToolExecutionNotStarted,
    #[error("subagent result does not match the suspended spawn request")]
    SubagentResultBindingMismatch,
    #[error("tool approval identity or binding does not match the reviewed request")]
    ApprovalBindingMismatch,
    #[error("tool approval decision is invalid: {0}")]
    InvalidApprovalDecision(String),
    #[error("tool approval decision has expired")]
    ApprovalDecisionExpired,
    #[error("tool turn is not complete enough for another model invocation")]
    ToolTurnIncomplete,
    #[error("execution budget has no capacity for another model invocation")]
    BudgetExhausted,
    #[error("tool executor configuration is invalid: {0}")]
    ToolExecutorConfiguration(String),
    #[error("worker checkpoint is invalid: {0}")]
    InvalidCheckpoint(String),
    #[error("worker checkpoint identity does not match the replacement command")]
    CheckpointIdentityMismatch,
    #[error("replacement owner epoch and fencing token do not advance the checkpoint lease")]
    StaleCheckpointLease,
    #[error("worker tool catalog does not match the checkpoint")]
    CheckpointToolCatalogMismatch,
    #[error("checkpoint contains an ambiguous non-idempotent tool execution")]
    AmbiguousToolExecution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadIdentityRenewalOutcome {
    Applied,
    Duplicate,
}

impl WorkerProcessor {
    pub fn apply_workload_identity_renewal(
        &mut self,
        command: WorkloadIdentityRenewalCommand,
        received_at: DateTime<Utc>,
        verifier: &WorkloadTokenVerifier,
    ) -> Result<WorkloadIdentityRenewalOutcome, WorkerAssignmentError> {
        command.validate().map_err(|error| {
            WorkerAssignmentError::InvalidWorkloadIdentityRenewal(error.to_string())
        })?;
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.worker_incarnation_id != self.worker_incarnation_id {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if command.issued_at > received_at || received_at >= command.lease_expires_at {
            return Err(WorkerAssignmentError::LeaseExpired);
        }

        let execution = self
            .accepted
            .get_mut(&command.attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let active = &execution.command;
        if command.tenant_id != active.tenant_id
            || command.run_id != active.run_id
            || command.owner_epoch != active.owner_epoch
            || command.fencing_token != active.fencing_token
        {
            return Err(WorkerAssignmentError::WorkloadIdentityBindingMismatch);
        }
        if command.generation <= execution.identity_generation {
            if command.generation == execution.identity_generation
                && command.workload_token == active.workload_token
                && command.lease_expires_at == active.lease_expires_at
            {
                return Ok(WorkloadIdentityRenewalOutcome::Duplicate);
            }
            return Err(WorkerAssignmentError::StaleWorkloadIdentityRenewal);
        }
        if command.lease_expires_at <= active.lease_expires_at {
            return Err(WorkerAssignmentError::StaleWorkloadIdentityRenewal);
        }

        let now_unix_ms = received_at.timestamp_millis();
        let capabilities = [
            RequiredCapability::new("model-gateway", "model.execute", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.read", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
        ];
        let mut verified_claims = None;
        for capability in capabilities {
            let claims = verifier
                .verify(command.workload_token.as_str(), capability, now_unix_ms)
                .map_err(|error| {
                    WorkerAssignmentError::InvalidWorkloadIdentityRenewal(error.to_string())
                })?;
            verified_claims.get_or_insert(claims);
        }
        let claims = verified_claims.expect("at least one capability is required");
        let binding = WorkloadIdentityBinding {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
        };
        if !claims.authorizes(&binding)
            || claims.model_policy_id != active.model_policy_id
            || claims.model_policy_digest != active.model_policy_digest
            || claims.issued_at_unix_ms != command.issued_at.timestamp_millis()
            || claims.expires_at_unix_ms != command.lease_expires_at.timestamp_millis()
        {
            return Err(WorkerAssignmentError::WorkloadIdentityBindingMismatch);
        }

        execution.command.workload_token = command.workload_token;
        execution.command.lease_expires_at = command.lease_expires_at;
        execution.identity_generation = command.generation;
        Ok(WorkloadIdentityRenewalOutcome::Applied)
    }

    pub fn new(
        worker_id: Uuid,
        placements: Vec<Placement>,
        capacity: u32,
        runtime_version: String,
    ) -> Result<Self, WorkerAssignmentError> {
        Self::new_with_incarnation(worker_id, worker_id, placements, capacity, runtime_version)
    }

    pub fn new_with_incarnation(
        worker_id: Uuid,
        worker_incarnation_id: Uuid,
        placements: Vec<Placement>,
        capacity: u32,
        runtime_version: String,
    ) -> Result<Self, WorkerAssignmentError> {
        if worker_id.is_nil()
            || worker_incarnation_id.is_nil()
            || placements.is_empty()
            || capacity == 0
            || runtime_version.trim().is_empty()
        {
            return Err(WorkerAssignmentError::InvalidConfiguration);
        }
        Ok(Self {
            worker_id,
            worker_incarnation_id,
            placements,
            capacity,
            runtime_version,
            admission_fence: WorkerAdmissionFence::new(),
            draining: None,
            accepted: HashMap::new(),
            completed: HashMap::new(),
            tool_registry: ToolRegistry::default(),
            tool_definitions: BTreeMap::new(),
            skill_artifact_verifier: None,
        })
    }

    /// Read-only view of the registered Tools.
    ///
    /// Shared so a test can ask what the kernel would decide, rather than
    /// asserting on the descriptor and calling that the same thing. It is the
    /// planning decision that matters, and only the registry makes it.
    pub fn tool_registry(&self) -> &ToolRegistry {
        &self.tool_registry
    }

    pub fn set_skill_artifact_verifier(&mut self, verifier: SkillArtifactVerifier) {
        self.skill_artifact_verifier = Some(verifier);
    }

    pub fn register_tool(
        &mut self,
        definition: WorkerToolDefinition,
    ) -> Result<(), WorkerAssignmentError> {
        if definition.description.trim().is_empty() || !definition.input_schema.is_object() {
            return Err(WorkerAssignmentError::ToolConfiguration(
                "tool description and object input schema are required".into(),
            ));
        }
        self.tool_registry
            .register(definition.descriptor.clone())
            .map_err(|error| WorkerAssignmentError::ToolConfiguration(error.to_string()))?;
        self.tool_definitions
            .insert(definition.descriptor.name.clone(), definition);
        Ok(())
    }

    pub fn validate_tool_executor(
        &self,
        tool_name: &str,
        sandbox: SandboxClass,
        implementation_digest: &str,
    ) -> Result<(), WorkerAssignmentError> {
        let definition = self.tool_definitions.get(tool_name).ok_or_else(|| {
            WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {tool_name} is not registered"
            ))
        })?;
        if definition.descriptor.sandbox != sandbox {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {tool_name} requires {:?}, not {:?}",
                definition.descriptor.sandbox, sandbox
            )));
        }
        if definition.descriptor.implementation_digest != implementation_digest {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {tool_name} executor implementation does not match its catalog"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub const fn worker_id(&self) -> Uuid {
        self.worker_id
    }

    #[must_use]
    pub const fn worker_incarnation_id(&self) -> Uuid {
        self.worker_incarnation_id
    }

    #[must_use]
    pub fn admission_fence(&self) -> WorkerAdmissionFence {
        self.admission_fence.clone()
    }

    /// Closes admission for this worker incarnation. The transition is one-way:
    /// the process must exit and return with a new incarnation to accept work again.
    pub fn begin_draining(
        &mut self,
        started_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<(), WorkerAssignmentError> {
        if deadline <= started_at {
            return Err(WorkerAssignmentError::InvalidDrainWindow);
        }
        self.admission_fence.close();
        let requested = DrainState {
            started_at,
            deadline,
        };
        match self.draining {
            None => {
                self.draining = Some(requested);
                Ok(())
            }
            Some(existing) if existing == requested => Ok(()),
            Some(_) => Err(WorkerAssignmentError::AlreadyDraining),
        }
    }

    #[must_use]
    pub const fn is_draining(&self) -> bool {
        self.draining.is_some()
    }

    #[must_use]
    pub fn active_attempt_ids(&self) -> Vec<Uuid> {
        let mut attempts = self.accepted.keys().copied().collect::<Vec<_>>();
        attempts.sort_unstable();
        attempts
    }

    #[must_use]
    fn checkpointable_attempt_ids(&self) -> Vec<Uuid> {
        let mut attempts = self
            .accepted
            .iter()
            .filter(|(_, execution)| {
                execution.terminal_event.is_none() && !execution.machine.status().is_terminal()
            })
            .map(|(attempt_id, _)| *attempt_id)
            .collect::<Vec<_>>();
        attempts.sort_unstable();
        attempts
    }

    pub fn accept(
        &mut self,
        command: RunExecutionCommand,
        accepted_at: DateTime<Utc>,
    ) -> Result<RunExecutionAccepted, WorkerAssignmentError> {
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.schema_version >= 2
            && command.worker_incarnation_id != self.worker_incarnation_id
        {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if let Some(existing) = self.accepted.get(&command.attempt_id) {
            if existing.accepted.run_id != command.run_id {
                return Err(WorkerAssignmentError::AttemptConflict);
            }
            return Ok(existing.accepted.clone());
        }
        if let Some(existing) = self.completed.get(&command.attempt_id) {
            if existing.run_id != command.run_id {
                return Err(WorkerAssignmentError::AttemptConflict);
            }
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        if !self.admission_fence.is_open() {
            return Err(WorkerAssignmentError::Draining);
        }
        command
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidCommand(error.to_string()))?;
        if accepted_at >= command.lease_expires_at {
            return Err(WorkerAssignmentError::LeaseExpired);
        }
        if self.capacity_consuming_attempts() >= self.capacity as usize {
            return Err(WorkerAssignmentError::AtCapacity);
        }
        let effective_skill_state = self.effective_skill_state(&command)?;

        let accepted = RunExecutionAccepted {
            schema_version: RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION,
            message_id: Uuid::now_v7(),
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: self.worker_id,
            worker_incarnation_id: self.worker_incarnation_id,
            accepted_at,
        };
        let machine = RunMachine::new(
            command.run_id,
            command.tenant_id,
            command.session_id,
            command.attempt_id,
        );
        let input = command.input.clone();
        let mut transcript = Vec::with_capacity(2);
        if !effective_skill_state.agent_instructions.is_empty() {
            transcript.push(ModelMessage {
                role: ModelRole::System as i32,
                content: vec![ContentPart {
                    body: Some(content_part::Body::Text(TextPart {
                        text: effective_skill_state.agent_instructions.clone(),
                    })),
                }],
            });
        }
        transcript.push(ModelMessage {
            role: ModelRole::User as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::Text(TextPart { text: input })),
            }],
        });
        self.accepted.insert(
            command.attempt_id,
            ActiveExecution {
                command,
                identity_generation: 1,
                accepted: accepted.clone(),
                machine,
                cancellation: CancellationToken::new(),
                started_event: None,
                terminal_event: None,
                transcript,
                effective_agent_instructions: effective_skill_state.agent_instructions,
                effective_tool_names: effective_skill_state.tool_names,
                effective_tool_catalog_digest: effective_skill_state.tool_catalog_digest,
                effective_skill_binding_digest: effective_skill_state.skill_binding_digest,
                pending_tool_calls: VecDeque::new(),
                outstanding_tool_calls: HashMap::new(),
                started_tool_calls: HashMap::new(),
                recovery_replanned_tools: HashMap::new(),
                rebound_approval_event: None,
                pending_approval: None,
                pending_subagent: None,
                subagent_result_receipt: None,
                steering_receipts: HashMap::new(),
                budget_usage: BudgetUsage::default(),
                pending_budget_exhaustion: None,
                approval_decisions: HashMap::new(),
                restored_from_checkpoint: None,
            },
        );
        Ok(accepted)
    }

    pub fn checkpoint(
        &self,
        attempt_id: Uuid,
    ) -> Result<agent_protocol::CheckpointSnapshot, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.machine.status().is_terminal() || execution.terminal_event.is_some() {
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        let state = WorkerCheckpointState {
            schema_version: WORKER_CHECKPOINT_SCHEMA_VERSION,
            runtime_version: self.runtime_version.clone(),
            workspace_id: execution.command.workspace_id,
            agent_version_id: execution.command.agent_version_id,
            model_policy_id: execution.command.model_policy_id,
            input_digest: digest_bytes(execution.command.input.as_bytes()),
            agent_instructions_digest: digest_bytes(
                execution.effective_agent_instructions.as_bytes(),
            ),
            skill_binding_digest: execution.effective_skill_binding_digest.clone(),
            lineage: execution.command.lineage.clone(),
            subagent_roles: execution.command.subagent_roles.clone(),
            budget: execution.command.budget.clone(),
            delegated_scopes: execution.command.delegated_scopes.clone(),
            owner_epoch: execution.command.owner_epoch,
            fencing_token: execution.command.fencing_token,
            tool_catalog_digest: execution.effective_tool_catalog_digest.clone(),
            transcript: execution
                .transcript
                .iter()
                .map(Message::encode_to_vec)
                .collect(),
            pending_tool_calls: execution.pending_tool_calls.iter().cloned().collect(),
            outstanding_tool_calls: execution
                .outstanding_tool_calls
                .iter()
                .map(|(id, request)| (id.clone(), request.clone()))
                .collect(),
            started_tool_calls: execution
                .started_tool_calls
                .iter()
                .map(|(id, event)| (id.clone(), event.clone()))
                .collect(),
            pending_approval: execution.pending_approval.clone(),
            pending_subagent: execution.pending_subagent.clone(),
            budget_usage: execution.budget_usage,
            pending_budget_exhaustion: execution.pending_budget_exhaustion,
            steering_receipts: execution
                .steering_receipts
                .iter()
                .map(|(id, receipt)| (*id, receipt.clone()))
                .collect(),
        };
        let state = serde_json::to_vec(&state)
            .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        Ok(execution.machine.checkpoint(state))
    }

    pub fn checkpoint_message(
        &self,
        attempt_id: Uuid,
        created_at: DateTime<Utc>,
    ) -> Result<RunCheckpointPublished, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let snapshot = self.checkpoint(attempt_id)?;
        Ok(RunCheckpointPublished::new(
            &snapshot,
            execution.command.owner_epoch,
            execution.command.fencing_token,
            execution.effective_tool_catalog_digest.clone(),
            created_at,
        ))
    }

    pub fn prepare_checkpoint_message(
        &self,
        attempt_id: Uuid,
        created_at: DateTime<Utc>,
    ) -> Result<PreparedRunCheckpoint, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let snapshot = self.checkpoint(attempt_id)?;
        RunCheckpointPublished::prepare_v2(
            &snapshot,
            execution.command.owner_epoch,
            execution.command.fencing_token,
            execution.effective_tool_catalog_digest.clone(),
            created_at,
        )
        .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))
    }

    pub fn checkpoint_store_context(
        &self,
        attempt_id: Uuid,
    ) -> Result<CheckpointStoreContext, WorkerAssignmentError> {
        let command = &self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?
            .command;
        Ok(CheckpointStoreContext {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
            workload_token: command.workload_token.as_str().to_owned(),
        })
    }

    pub fn restore(
        &mut self,
        command: RunExecutionCommand,
        checkpoint: agent_protocol::CheckpointSnapshot,
        restored_at: DateTime<Utc>,
    ) -> Result<WorkerRestoreReceipt, WorkerAssignmentError> {
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.schema_version >= 2
            && command.worker_incarnation_id != self.worker_incarnation_id
        {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if let Some(existing) = self.accepted.get(&command.attempt_id) {
            if existing.command == command
                && existing.restored_from_checkpoint.as_deref() == Some(&checkpoint.digest)
            {
                return Ok(WorkerRestoreReceipt {
                    accepted: existing.accepted.clone(),
                    event: existing
                        .started_event
                        .clone()
                        .expect("restored execution always has a restored event"),
                });
            }
            return Err(WorkerAssignmentError::AttemptConflict);
        }
        if !self.admission_fence.is_open() {
            return Err(WorkerAssignmentError::Draining);
        }
        command
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidCommand(error.to_string()))?;
        if restored_at >= command.lease_expires_at {
            return Err(WorkerAssignmentError::LeaseExpired);
        }
        if self.capacity_consuming_attempts() >= self.capacity as usize {
            return Err(WorkerAssignmentError::AtCapacity);
        }
        if self.completed.contains_key(&command.attempt_id) {
            return Err(WorkerAssignmentError::AttemptConflict);
        }
        let effective_skill_state = self.effective_skill_state(&command)?;
        if checkpoint.status.is_terminal() || checkpoint.attempt_id == command.attempt_id {
            return Err(WorkerAssignmentError::CheckpointIdentityMismatch);
        }
        let state: WorkerCheckpointState = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        if !(1..=WORKER_CHECKPOINT_SCHEMA_VERSION).contains(&state.schema_version) {
            return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                "unsupported schema version {}",
                state.schema_version
            )));
        }
        if checkpoint.tenant_id != command.tenant_id
            || checkpoint.run_id != command.run_id
            || checkpoint.session_id != command.session_id
            || state.workspace_id != command.workspace_id
            || state.agent_version_id != command.agent_version_id
            || state.model_policy_id != command.model_policy_id
            || state.input_digest != digest_bytes(command.input.as_bytes())
            || state.agent_instructions_digest
                != digest_bytes(effective_skill_state.agent_instructions.as_bytes())
            || (state.schema_version >= 5
                && state.skill_binding_digest != effective_skill_state.skill_binding_digest)
            || state.lineage != command.lineage
            || (state.schema_version >= 3 && state.subagent_roles != command.subagent_roles)
            || state.budget != command.budget
            || state.delegated_scopes != command.delegated_scopes
        {
            return Err(WorkerAssignmentError::CheckpointIdentityMismatch);
        }
        if command.owner_epoch <= state.owner_epoch || command.fencing_token == state.fencing_token
        {
            return Err(WorkerAssignmentError::StaleCheckpointLease);
        }
        // Compare against the rule that produced the stored digest. Checkpoints
        // below schema 5 were written before Tool activation was narrowed by the
        // delegated scopes, so holding them to the current rule would strand
        // every in-flight run that has an out-of-scope Tool installed. The run
        // still resumes under the narrowed catalog either way; only the
        // comparison is version aware.
        let expected_tool_catalog_digest = if state.schema_version >= 5 {
            &effective_skill_state.tool_catalog_digest
        } else {
            &effective_skill_state.legacy_tool_catalog_digest
        };
        if state.tool_catalog_digest != *expected_tool_catalog_digest {
            return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
        }
        for tool_call_id in state.started_tool_calls.keys() {
            let Some(request) = state.outstanding_tool_calls.get(tool_call_id) else {
                return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                    "started tool {tool_call_id} has no execution request"
                )));
            };
            if matches!(
                request.effect,
                agent_protocol::ToolEffect::NonIdempotent | agent_protocol::ToolEffect::Unknown
            ) {
                return Err(WorkerAssignmentError::AmbiguousToolExecution);
            }
        }
        let transcript = state
            .transcript
            .iter()
            .map(|encoded| {
                ModelMessage::decode(encoded.as_slice())
                    .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut machine =
            RunMachine::from_checkpoint_for_attempt(checkpoint.clone(), command.attempt_id)
                .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        let event = machine
            .record_restored(&checkpoint.digest)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        let accepted = RunExecutionAccepted {
            schema_version: RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION,
            message_id: Uuid::now_v7(),
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: self.worker_id,
            worker_incarnation_id: self.worker_incarnation_id,
            accepted_at: restored_at,
        };
        self.accepted.insert(
            command.attempt_id,
            ActiveExecution {
                command,
                identity_generation: 1,
                accepted: accepted.clone(),
                machine,
                cancellation: CancellationToken::new(),
                started_event: Some(event.clone()),
                terminal_event: None,
                transcript,
                effective_agent_instructions: effective_skill_state.agent_instructions,
                effective_tool_names: effective_skill_state.tool_names,
                effective_tool_catalog_digest: effective_skill_state.tool_catalog_digest,
                effective_skill_binding_digest: effective_skill_state.skill_binding_digest,
                pending_tool_calls: state.pending_tool_calls.into(),
                outstanding_tool_calls: state.outstanding_tool_calls.into_iter().collect(),
                started_tool_calls: HashMap::new(),
                recovery_replanned_tools: HashMap::new(),
                rebound_approval_event: None,
                pending_approval: state.pending_approval,
                pending_subagent: state.pending_subagent,
                subagent_result_receipt: None,
                steering_receipts: state.steering_receipts.into_iter().collect(),
                budget_usage: state.budget_usage,
                pending_budget_exhaustion: state.pending_budget_exhaustion,
                approval_decisions: HashMap::new(),
                restored_from_checkpoint: Some(checkpoint.digest),
            },
        );
        Ok(WorkerRestoreReceipt { accepted, event })
    }

    pub fn recovery_action(
        &self,
        attempt_id: Uuid,
    ) -> Result<WorkerRecoveryAction, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(exhaustion) = execution.pending_budget_exhaustion {
            return Ok(WorkerRecoveryAction::TerminateBudgetExceeded(
                exhaustion.dimension,
            ));
        }
        if execution.pending_approval.is_some() {
            return Ok(WorkerRecoveryAction::WaitForApproval);
        }
        if execution.pending_subagent.is_some() {
            return Ok(WorkerRecoveryAction::WaitForSubagent);
        }
        if !execution.pending_tool_calls.is_empty() {
            return Ok(WorkerRecoveryAction::PlanPendingTool);
        }
        if execution.outstanding_tool_calls.len() > 1 {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "serial worker checkpoint contains multiple outstanding tools".into(),
            ));
        }
        Ok(execution
            .outstanding_tool_calls
            .values()
            .next()
            .cloned()
            .map(WorkerRecoveryAction::RetryTool)
            .unwrap_or(WorkerRecoveryAction::InvokeModel))
    }

    pub fn replan_recovered_tool(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(event) = execution.recovery_replanned_tools.get(tool_call_id) {
            return Ok(event.clone());
        }
        let request = execution
            .outstanding_tool_calls
            .get(tool_call_id)
            .cloned()
            .ok_or(WorkerAssignmentError::ToolCallNotExecuting)?;
        let event = execution
            .machine
            .apply_tool_plan(&ToolPlan::Execute(request))
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution
            .recovery_replanned_tools
            .insert(tool_call_id.to_owned(), event.clone());
        Ok(event)
    }

    pub fn rebind_recovered_approval(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(event) = &execution.rebound_approval_event {
            return Ok(event.clone());
        }
        let approval = execution
            .pending_approval
            .as_ref()
            .ok_or(WorkerAssignmentError::ApprovalBindingMismatch)?;
        let event = execution
            .machine
            .record_approval_rebound(approval)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.rebound_approval_event = Some(event.clone());
        Ok(event)
    }

    fn effective_skill_state(
        &self,
        command: &RunExecutionCommand,
    ) -> Result<EffectiveSkillState, WorkerAssignmentError> {
        if command.schema_version < 5 {
            // Before Skills existed the whole preinstalled catalog was
            // effective, and the legacy digest covered it without regard to
            // scopes.
            let declared_tool_names = self
                .tool_definitions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            let tool_names = declared_tool_names
                .iter()
                .filter(|name| self.is_within_delegated_scopes(name, command))
                .cloned()
                .collect::<BTreeSet<_>>();
            return Ok(EffectiveSkillState {
                agent_instructions: command.agent_instructions.clone(),
                tool_catalog_digest: self.tool_catalog_digest_for(&tool_names),
                legacy_tool_catalog_digest: self.tool_catalog_digest_for(&declared_tool_names),
                tool_names,
                skill_binding_digest: skill_binding_digest(&[]),
            });
        }
        if command.skill_snapshots.is_empty() {
            let tool_names = BTreeSet::new();
            let digest = self.tool_catalog_digest_for(&tool_names);
            return Ok(EffectiveSkillState {
                agent_instructions: command.agent_instructions.clone(),
                tool_catalog_digest: digest.clone(),
                legacy_tool_catalog_digest: digest,
                tool_names,
                skill_binding_digest: skill_binding_digest(&[]),
            });
        }
        let verifier = self
            .skill_artifact_verifier
            .as_ref()
            .ok_or(WorkerAssignmentError::InvalidSkillArtifact)?;
        let current_platform = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "darwin-arm64"
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            "linux-arm64"
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            "linux-x86_64"
        } else {
            return Err(WorkerAssignmentError::InvalidSkillArtifact);
        };
        let runtime_version = semver::Version::parse(&self.runtime_version)
            .map_err(|_| WorkerAssignmentError::InvalidConfiguration)?;
        let mut agent_instructions = command.agent_instructions.clone();
        let mut tool_names = BTreeSet::new();
        let mut declared_tool_names = BTreeSet::new();
        for skill in &command.skill_snapshots {
            let minimum = semver::Version::parse(&skill.min_runtime_version)
                .map_err(|_| WorkerAssignmentError::InvalidSkillArtifact)?;
            if !verifier.verify(skill)
                || minimum > runtime_version
                || !skill
                    .supported_platforms
                    .iter()
                    .any(|platform| platform == current_platform)
            {
                return Err(WorkerAssignmentError::InvalidSkillArtifact);
            }
            agent_instructions.push_str("\n\n[Skill ");
            agent_instructions.push_str(&skill.name);
            agent_instructions.push('@');
            agent_instructions.push_str(&skill.semantic_version);
            agent_instructions.push_str("]\n");
            agent_instructions.push_str(&skill.instructions);
            for tool_name in &skill.tool_names {
                if !self.tool_definitions.contains_key(tool_name) {
                    return Err(WorkerAssignmentError::ToolConfiguration(format!(
                        "Skill {} requires unavailable trusted tool {tool_name}",
                        skill.name
                    )));
                }
                declared_tool_names.insert(tool_name.clone());
                // A Skill declaration only narrows authority. A declared Tool
                // that reaches outside the AgentVersion delegated scopes stays
                // inactive instead of widening what this run may call.
                if self.is_within_delegated_scopes(tool_name, command) {
                    tool_names.insert(tool_name.clone());
                }
            }
        }
        Ok(EffectiveSkillState {
            agent_instructions,
            tool_catalog_digest: self.tool_catalog_digest_for(&tool_names),
            legacy_tool_catalog_digest: self.tool_catalog_digest_for(&declared_tool_names),
            tool_names,
            skill_binding_digest: skill_binding_digest(&command.skill_snapshots),
        })
    }

    fn is_within_delegated_scopes(&self, tool_name: &str, command: &RunExecutionCommand) -> bool {
        self.tool_registry
            .authorize(tool_name, &command.delegated_scopes)
            .is_ok()
    }

    fn tool_catalog_digest_for(&self, tool_names: &BTreeSet<String>) -> String {
        let definitions = tool_names
            .iter()
            .filter_map(|name| self.tool_definitions.get_key_value(name))
            .collect::<BTreeMap<_, _>>();
        let material = serde_json::to_vec(&definitions)
            .expect("effective worker tool catalog is serializable");
        digest_bytes(&material)
    }

    pub fn start(&mut self, attempt_id: Uuid) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(event) = &execution.started_event {
            return Ok(event.clone());
        }
        let event = execution
            .machine
            .apply(RunCommand::Start)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.started_event = Some(event.clone());
        Ok(event)
    }

    pub fn apply_model_event(
        &mut self,
        attempt_id: Uuid,
        model_event: ModelStreamEvent,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        if self.completed.contains_key(&attempt_id) {
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.terminal_event.is_some() {
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        if let Some(exhaustion) = execution.pending_budget_exhaustion
            && matches!(
                model_event,
                ModelStreamEvent::Completed {
                    reason: ModelFinishReason::ToolCalls
                }
            )
        {
            return Self::terminate_budget_exhaustion(execution, exhaustion.dimension);
        }
        if let Some(exhaustion) = execution.pending_budget_exhaustion {
            if exhaustion.exceeded {
                return Self::terminate_budget_exhaustion(execution, exhaustion.dimension);
            }
            if matches!(
                model_event,
                ModelStreamEvent::Completed {
                    reason: ModelFinishReason::Stop
                }
            ) {
                execution.pending_budget_exhaustion = None;
            }
        }
        if let ModelStreamEvent::ToolCall {
            id,
            name,
            arguments: _,
        } = &model_event
            && (id.trim().is_empty()
                || name.trim().is_empty()
                || execution
                    .pending_tool_calls
                    .iter()
                    .any(|call| call.id == *id)
                || execution.outstanding_tool_calls.contains_key(id))
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        if matches!(
            model_event,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls
            }
        ) && execution.pending_tool_calls.is_empty()
        {
            return Err(WorkerAssignmentError::EmptyToolTurn);
        }
        let retained_model_event = model_event.clone();
        let event = execution
            .machine
            .apply_model_event(model_event)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        match retained_model_event {
            ModelStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cost_micros,
            } => {
                execution.budget_usage.tokens = execution
                    .budget_usage
                    .tokens
                    .saturating_add(input_tokens.saturating_add(output_tokens));
                execution.budget_usage.cost_micros = execution
                    .budget_usage
                    .cost_micros
                    .saturating_add(cost_micros);
                execution.pending_budget_exhaustion =
                    budget_exhaustion(execution.budget_usage, &execution.command.budget);
            }
            ModelStreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => execution.pending_tool_calls.push_back(ToolCall {
                id,
                name,
                arguments,
            }),
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            } => execution.transcript.push(ModelMessage {
                role: ModelRole::Assistant as i32,
                content: execution
                    .pending_tool_calls
                    .iter()
                    .map(|call| ContentPart {
                        body: Some(content_part::Body::ToolCall(ToolCallPart {
                            tool_call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments_json: serde_json::to_vec(&call.arguments)
                                .expect("tool call arguments are serializable"),
                        })),
                    })
                    .collect(),
            }),
            _ => {}
        }
        if execution.machine.status().is_terminal() {
            execution.terminal_event = Some(event.clone());
        }
        Ok(event)
    }

    pub fn pending_budget_exhaustion(
        &self,
        attempt_id: Uuid,
    ) -> Result<Option<BudgetDimension>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| {
                execution
                    .pending_budget_exhaustion
                    .map(|exhaustion| exhaustion.dimension)
            })
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    fn hard_budget_exhaustion(
        &self,
        attempt_id: Uuid,
    ) -> Result<Option<BudgetDimension>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| {
                execution
                    .pending_budget_exhaustion
                    .filter(|exhaustion| exhaustion.exceeded)
                    .map(|exhaustion| exhaustion.dimension)
            })
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn terminate_pending_budget_exhaustion(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<Option<EventEnvelope>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(terminal) = &execution.terminal_event {
            return Ok(Some(terminal.clone()));
        }
        let Some(exhaustion) = execution.pending_budget_exhaustion else {
            return Ok(None);
        };
        Self::terminate_budget_exhaustion(execution, exhaustion.dimension).map(Some)
    }

    fn terminate_budget_exhaustion(
        execution: &mut ActiveExecution,
        dimension: BudgetDimension,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        execution.cancellation.cancel();
        execution.pending_budget_exhaustion = None;
        execution.pending_tool_calls.clear();
        let event = execution
            .machine
            .record_budget_exhausted(dimension)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.terminal_event = Some(event.clone());
        Ok(event)
    }

    pub fn cancel(&mut self, attempt_id: Uuid) -> Result<EventEnvelope, WorkerAssignmentError> {
        if let Some(completed) = self.completed.get(&attempt_id) {
            return Ok(completed.terminal_event.clone());
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(terminal) = &execution.terminal_event {
            return Ok(terminal.clone());
        }
        execution.cancellation.cancel();
        let event = execution
            .machine
            .apply(RunCommand::Cancel)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.terminal_event = Some(event.clone());
        Ok(event)
    }

    pub fn cancellation_token(
        &self,
        attempt_id: Uuid,
    ) -> Result<CancellationToken, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.cancellation.clone())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn attempt_is_terminal(&self, attempt_id: Uuid) -> Result<bool, WorkerAssignmentError> {
        if self.completed.contains_key(&attempt_id) {
            return Ok(true);
        }
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.terminal_event.is_some())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn status(
        &self,
        attempt_id: Uuid,
    ) -> Result<agent_protocol::RunStatus, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.machine.status())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    fn has_pending_tool_calls(&self, attempt_id: Uuid) -> Result<bool, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| !execution.pending_tool_calls.is_empty())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn prepare_model_invocation(
        &self,
        attempt_id: Uuid,
    ) -> Result<PreparedModelInvocation, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.pending_approval.is_some()
            || execution.pending_subagent.is_some()
            || !execution.pending_tool_calls.is_empty()
            || !execution.outstanding_tool_calls.is_empty()
        {
            return Err(WorkerAssignmentError::ToolTurnIncomplete);
        }
        if execution.pending_budget_exhaustion.is_some() {
            return Err(WorkerAssignmentError::BudgetExhausted);
        }
        let command = &execution.command;
        let remaining_tokens = command
            .budget
            .max_tokens
            .saturating_sub(execution.budget_usage.tokens);
        if remaining_tokens == 0 {
            return Err(WorkerAssignmentError::BudgetExhausted);
        }
        let model_policy_snapshot_json = if command.model_policy_snapshot_base64.is_empty() {
            Vec::new()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(&command.model_policy_snapshot_base64)
                .map_err(|_| {
                    WorkerAssignmentError::InvalidCommand(
                        "model policy snapshot is not valid Base64".into(),
                    )
                })?
        };
        let mut tools = self
            .tool_definitions
            .values()
            .filter(|definition| {
                execution
                    .effective_tool_names
                    .contains(&definition.descriptor.name)
                    && self
                        .tool_registry
                        .authorize(&definition.descriptor.name, &command.delegated_scopes)
                        .is_ok()
            })
            .map(|definition| ModelTool {
                name: definition.descriptor.name.clone(),
                description: definition.description.clone(),
                input_schema_json: serde_json::to_vec(&definition.input_schema)
                    .expect("tool input schema is serializable"),
            })
            .collect::<Vec<_>>();
        if !command.subagent_roles.is_empty() && command.delegated_scopes.contains("agent:spawn") {
            tools.push(subagent_spawn_tool(&command.subagent_roles));
        }
        Ok(PreparedModelInvocation {
            invocation: ModelInvocation {
                schema_version: if model_policy_snapshot_json.is_empty() {
                    2
                } else {
                    3
                },
                tenant_id: command.tenant_id.to_string(),
                run_id: command.run_id.to_string(),
                session_id: command.session_id.to_string(),
                attempt_id: command.attempt_id.to_string(),
                worker_id: command.worker_id.to_string(),
                model_policy_id: command.model_policy_id.to_string(),
                expires_at_unix_ms: command.lease_expires_at.timestamp_millis(),
                messages: execution.transcript.clone(),
                tools,
                output_schema_json: Vec::new(),
                reasoning: ReasoningPolicy::Balanced as i32,
                max_output_tokens: remaining_tokens,
                worker_incarnation_id: command.worker_incarnation_id.to_string(),
                model_policy_snapshot_json,
                model_policy_digest: command.model_policy_digest.clone(),
            },
            workload_token: command.workload_token.clone(),
        })
    }

    pub fn plan_next_tool_call(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<PlannedToolCall, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.pending_approval.is_some() {
            return Err(WorkerAssignmentError::ToolTurnIncomplete);
        }
        let call = execution
            .pending_tool_calls
            .front()
            .cloned()
            .ok_or(WorkerAssignmentError::NoPendingToolCall)?;
        if call.name == "agent.spawn" {
            let arguments: SubagentSpawnArguments = serde_json::from_value(call.arguments.clone())
                .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            let role = execution
                .command
                .subagent_roles
                .iter()
                .find(|role| role.name == arguments.role)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            let used_cost_cents = execution.budget_usage.cost_micros.saturating_add(9_999) / 10_000;
            let remaining_tokens = execution
                .command
                .budget
                .max_tokens
                .saturating_sub(execution.budget_usage.tokens);
            let remaining_cost_cents = execution
                .command
                .budget
                .max_cost_cents
                .saturating_sub(used_cost_cents);
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || role
                    .delegated_scopes
                    .iter()
                    .any(|scope| !execution.command.delegated_scopes.contains(scope))
                || arguments.max_tokens > remaining_tokens
                || arguments.max_cost_cents > remaining_cost_cents
                || arguments.max_duration_seconds > execution.command.budget.max_duration_seconds
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let delegation_id = deterministic_delegation_id(&execution.command, &call.id);
            let binding_digest =
                subagent_binding_digest(&execution.command, &call, delegation_id, &arguments);
            let request = SubagentSpawnRequest {
                tool_call_id: call.id.clone(),
                delegation_id,
                role: arguments.role,
                input: arguments.input,
                budget: agent_protocol::RunBudget {
                    max_tokens: arguments.max_tokens,
                    max_cost_cents: arguments.max_cost_cents,
                    max_duration_seconds: arguments.max_duration_seconds,
                },
                binding_digest,
            };
            if !request.is_well_formed() {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .record_subagent_spawn_requested(&request)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution.pending_subagent = Some(request.clone());
            return Ok(PlannedToolCall {
                plan: ToolPlan::SubagentSpawn(request.clone()),
                event,
                followup_event: None,
                subagent_request: Some(request),
            });
        }
        if !execution.effective_tool_names.contains(&call.name) {
            return Err(WorkerAssignmentError::ToolConfiguration(format!(
                "tool {} is not activated by the execution Skill snapshot",
                call.name
            )));
        }
        let plan = self
            .tool_registry
            .plan(
                call,
                &execution.command.delegated_scopes,
                // Straight from the command: the Worker applies the tenant's
                // policy rather than one of its own.
                &execution.command.tool_approval_policies,
            )
            .map_err(|error| WorkerAssignmentError::ToolConfiguration(error.to_string()))?;
        execution.pending_tool_calls.pop_front();
        let event = execution
            .machine
            .apply_tool_plan(&plan)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        let followup_event = match &plan {
            ToolPlan::Execute(request) => {
                execution
                    .outstanding_tool_calls
                    .insert(request.call.id.clone(), request.clone());
                None
            }
            // Same bookkeeping as Execute -- it is going to run -- but a
            // separate arm so the exemption cannot be folded back into the
            // ordinary path by accident.
            ToolPlan::AutoApproved {
                execution: request, ..
            } => {
                execution
                    .outstanding_tool_calls
                    .insert(request.call.id.clone(), request.clone());
                None
            }
            ToolPlan::ApprovalRequired(approval) => {
                execution.pending_approval = Some(approval.clone());
                None
            }
            ToolPlan::Denied(request) => {
                let content = serde_json::json!({
                    "error": {
                        "code": "tool_policy_denied",
                        "message": "tool execution is denied by runtime policy"
                    }
                });
                let result = execution
                    .machine
                    .record_tool_result(
                        &request.call.id,
                        &request.binding_digest,
                        content.clone(),
                        true,
                    )
                    .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
                execution.transcript.push(ModelMessage {
                    role: ModelRole::Tool as i32,
                    content: vec![ContentPart {
                        body: Some(content_part::Body::ToolResult(ToolResultPart {
                            tool_call_id: request.call.id.clone(),
                            content_json: serde_json::to_vec(&content)
                                .expect("tool denial content is serializable"),
                        })),
                    }],
                });
                Some(result)
            }
            ToolPlan::SubagentSpawn(_) => unreachable!("subagent spawn is planned separately"),
        };
        Ok(PlannedToolCall {
            plan,
            event,
            followup_event,
            subagent_request: None,
        })
    }

    pub fn approve_tool_call(
        &mut self,
        attempt_id: Uuid,
        approval_id: Uuid,
        binding_digest: &str,
    ) -> Result<(EventEnvelope, ToolExecutionRequest), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let approval = execution
            .pending_approval
            .as_ref()
            .filter(|approval| {
                approval.approval_id == approval_id
                    && approval.execution.binding_digest == binding_digest
            })
            .cloned()
            .ok_or(WorkerAssignmentError::ApprovalBindingMismatch)?;
        let event = execution
            .machine
            .apply(RunCommand::Approve)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.pending_approval = None;
        execution.outstanding_tool_calls.insert(
            approval.execution.call.id.clone(),
            approval.execution.clone(),
        );
        Ok((event, approval.execution))
    }

    pub fn apply_tool_approval(
        &mut self,
        command: ToolApprovalDecisionCommand,
        received_at: DateTime<Utc>,
    ) -> Result<ToolApprovalOutcome, WorkerAssignmentError> {
        command
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidApprovalDecision(error.to_string()))?;
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.schema_version >= 2
            && command.worker_incarnation_id != self.worker_incarnation_id
        {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if received_at >= command.expires_at {
            return Err(WorkerAssignmentError::ApprovalDecisionExpired);
        }
        let execution = self
            .accepted
            .get_mut(&command.attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.command.tenant_id != command.tenant_id
            || execution.command.run_id != command.run_id
        {
            return Err(WorkerAssignmentError::AttemptConflict);
        }
        if let Some(receipt) = execution.approval_decisions.get(&command.approval_id) {
            return if receipt.command == command {
                Ok(receipt.outcome.clone())
            } else {
                Err(WorkerAssignmentError::ApprovalBindingMismatch)
            };
        }
        let approval = execution
            .pending_approval
            .as_ref()
            .filter(|approval| {
                approval.approval_id == command.approval_id
                    && approval.execution.binding_digest == command.binding_digest
            })
            .cloned()
            .ok_or(WorkerAssignmentError::ApprovalBindingMismatch)?;
        let resumed = execution
            .machine
            .apply(RunCommand::Approve)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.pending_approval = None;

        let outcome = match command.decision {
            ToolApprovalDecision::AllowOnce => {
                execution.outstanding_tool_calls.insert(
                    approval.execution.call.id.clone(),
                    approval.execution.clone(),
                );
                ToolApprovalOutcome {
                    events: vec![resumed],
                    execution: Some(approval.execution),
                }
            }
            ToolApprovalDecision::Deny => {
                let tool_call_id = approval.execution.call.id;
                let content = serde_json::json!({
                    "error": {
                        "code": "approval_denied",
                        "message": "tool execution was denied by a reviewer"
                    }
                });
                let denied = execution
                    .machine
                    .record_tool_result(
                        &tool_call_id,
                        &approval.execution.binding_digest,
                        content.clone(),
                        true,
                    )
                    .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
                execution.transcript.push(ModelMessage {
                    role: ModelRole::Tool as i32,
                    content: vec![ContentPart {
                        body: Some(content_part::Body::ToolResult(ToolResultPart {
                            tool_call_id,
                            content_json: serde_json::to_vec(&content)
                                .expect("tool denial content is serializable"),
                        })),
                    }],
                });
                ToolApprovalOutcome {
                    events: vec![resumed, denied],
                    execution: None,
                }
            }
        };
        execution.approval_decisions.insert(
            command.approval_id,
            ApprovalDecisionReceipt {
                command,
                outcome: outcome.clone(),
            },
        );
        Ok(outcome)
    }

    pub fn record_tool_result(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: String,
        content: serde_json::Value,
        is_error: bool,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let binding_digest = self
            .accepted
            .get(&attempt_id)
            .and_then(|execution| execution.outstanding_tool_calls.get(&tool_call_id))
            .map(|request| request.binding_digest.clone())
            .ok_or(WorkerAssignmentError::ToolCallNotExecuting)?;
        self.record_bound_tool_result(attempt_id, tool_call_id, &binding_digest, content, is_error)
    }

    pub fn record_subagent_result(
        &mut self,
        attempt_id: Uuid,
        result: &SubagentResultDelivery,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some((digest, event)) = &execution.subagent_result_receipt {
            return if digest == &result.digest {
                Ok(event.clone())
            } else {
                Err(WorkerAssignmentError::SubagentResultBindingMismatch)
            };
        }
        let request = execution
            .pending_subagent
            .as_ref()
            .filter(|request| {
                request.tool_call_id == result.tool_call_id
                    && request.delegation_id == result.delegation_id
                    && request.binding_digest == result.binding_digest
            })
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        if !result.verify_digest() {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        let tool_call_id = request.tool_call_id.clone();
        let event = execution
            .machine
            .record_subagent_result_received(result)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.pending_subagent = None;
        execution.transcript.push(ModelMessage {
            role: ModelRole::Tool as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::ToolResult(ToolResultPart {
                    tool_call_id,
                    content_json: serde_json::to_vec(&result.content)
                        .expect("validated subagent result content is serializable"),
                })),
            }],
        });
        execution.subagent_result_receipt = Some((result.digest.clone(), event.clone()));
        Ok(event)
    }

    pub fn record_tool_execution_started(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
        binding_digest: &str,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request = execution
            .outstanding_tool_calls
            .get(tool_call_id)
            .filter(|request| request.binding_digest == binding_digest)
            .ok_or(WorkerAssignmentError::ToolExecutionBindingMismatch)?;
        if let Some(event) = execution.started_tool_calls.get(tool_call_id) {
            return Ok(event.clone());
        }
        let event = execution
            .machine
            .record_tool_execution_started(request)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution
            .started_tool_calls
            .insert(tool_call_id.to_owned(), event.clone());
        Ok(event)
    }

    pub fn record_bound_tool_result(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: &str,
        content: serde_json::Value,
        is_error: bool,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let Some(request) = execution.outstanding_tool_calls.get(&tool_call_id) else {
            return Err(WorkerAssignmentError::ToolCallNotExecuting);
        };
        if request.binding_digest != binding_digest {
            return Err(WorkerAssignmentError::ToolResultBindingMismatch);
        }
        if !execution.started_tool_calls.contains_key(&tool_call_id) {
            return Err(WorkerAssignmentError::ToolExecutionNotStarted);
        }
        execution.outstanding_tool_calls.remove(&tool_call_id);
        execution.started_tool_calls.remove(&tool_call_id);
        let event = execution
            .machine
            .record_tool_result(&tool_call_id, binding_digest, content.clone(), is_error)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.transcript.push(ModelMessage {
            role: ModelRole::Tool as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::ToolResult(ToolResultPart {
                    tool_call_id,
                    content_json: serde_json::to_vec(&content)
                        .expect("tool result content is serializable"),
                })),
            }],
        });
        Ok(event)
    }

    fn pending_approval_execution(
        &self,
        command: &ToolApprovalDecisionCommand,
    ) -> Result<ToolExecutionRequest, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&command.attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if let Some(receipt) = execution.approval_decisions.get(&command.approval_id) {
            return if receipt.command == *command {
                receipt
                    .outcome
                    .execution
                    .clone()
                    .ok_or(WorkerAssignmentError::ApprovalBindingMismatch)
            } else {
                Err(WorkerAssignmentError::ApprovalBindingMismatch)
            };
        }
        let approval = execution
            .pending_approval
            .as_ref()
            .filter(|approval| {
                approval.approval_id == command.approval_id
                    && approval.execution.binding_digest == command.binding_digest
            })
            .ok_or(WorkerAssignmentError::ApprovalBindingMismatch)?;
        Ok(approval.execution.clone())
    }

    fn tool_execution_context(
        &self,
        attempt_id: Uuid,
        workspace_base: &Path,
        requested_at: DateTime<Utc>,
    ) -> Result<ToolExecutionContext, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if !workspace_base.is_absolute() {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(
                "workspace base must be an absolute path".into(),
            ));
        }
        let remaining = execution
            .command
            .lease_expires_at
            .signed_duration_since(requested_at)
            .to_std()
            .map_err(|_| WorkerAssignmentError::LeaseExpired)?;
        let timeout = remaining.min(Duration::from_secs(300));
        if timeout.is_zero() {
            return Err(WorkerAssignmentError::LeaseExpired);
        }
        let workspace_root = materialize_native_workspace(
            workspace_base,
            execution.command.tenant_id,
            execution.command.workspace_id,
        )?;
        Ok(ToolExecutionContext {
            tenant_id: execution.command.tenant_id,
            run_id: execution.command.run_id,
            attempt_id,
            workspace_root,
            timeout,
            cancellation: execution.cancellation.clone(),
            requested_at,
        })
    }

    pub fn apply_cancellation(
        &mut self,
        command: RunCancellationCommand,
        received_at: DateTime<Utc>,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        command
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidCancellation(error.to_string()))?;
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.schema_version >= 2
            && command.worker_incarnation_id != self.worker_incarnation_id
        {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if received_at >= command.expires_at {
            return Err(WorkerAssignmentError::CancellationExpired);
        }
        if let Some(execution) = self.accepted.get(&command.attempt_id) {
            if execution.command.tenant_id != command.tenant_id
                || execution.command.run_id != command.run_id
            {
                return Err(WorkerAssignmentError::AttemptConflict);
            }
        } else if let Some(completed) = self.completed.get(&command.attempt_id) {
            if completed.run_id != command.run_id {
                return Err(WorkerAssignmentError::AttemptConflict);
            }
        } else {
            return Err(WorkerAssignmentError::UnknownAttempt);
        }
        self.cancel(command.attempt_id)
    }

    pub fn apply_steering(
        &mut self,
        command: RunSteeringCommand,
        received_at: DateTime<Utc>,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        command
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidSteering(error.to_string()))?;
        if command.worker_id != self.worker_id {
            return Err(WorkerAssignmentError::WrongWorker);
        }
        if command.worker_incarnation_id != self.worker_incarnation_id {
            return Err(WorkerAssignmentError::WrongWorkerIncarnation);
        }
        if received_at >= command.expires_at {
            return Err(WorkerAssignmentError::SteeringExpired);
        }
        let execution = self
            .accepted
            .get_mut(&command.attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.command.tenant_id != command.tenant_id
            || execution.command.run_id != command.run_id
        {
            return Err(WorkerAssignmentError::AttemptConflict);
        }
        if let Some(receipt) = execution
            .steering_receipts
            .get(&command.steering_id)
            .cloned()
        {
            if receipt.input_digest != command.input_digest {
                return Err(WorkerAssignmentError::SteeringConflict);
            }
            if receipt.event.attempt_id == command.attempt_id {
                return Ok(receipt.event);
            }
            let event = execution
                .machine
                .record_steering_applied(command.steering_id, &command.input_digest)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution.steering_receipts.insert(
                command.steering_id,
                SteeringReceipt {
                    input_digest: command.input_digest,
                    event: event.clone(),
                },
            );
            return Ok(event);
        }
        if received_at >= execution.command.lease_expires_at {
            return Err(WorkerAssignmentError::LeaseExpired);
        }
        if execution.terminal_event.is_some() || execution.machine.status().is_terminal() {
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        if execution.machine.status() != agent_protocol::RunStatus::Running
            || execution.pending_approval.is_some()
            || execution.pending_subagent.is_some()
            || !execution.pending_tool_calls.is_empty()
            || !execution.outstanding_tool_calls.is_empty()
            || !execution.started_tool_calls.is_empty()
        {
            return Err(WorkerAssignmentError::SteeringUnsafe);
        }
        execution.cancellation.cancel();
        execution.cancellation = CancellationToken::new();
        execution.transcript.push(ModelMessage {
            role: ModelRole::User as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::Text(TextPart {
                    text: command.input,
                })),
            }],
        });
        let event = execution
            .machine
            .record_steering_applied(command.steering_id, &command.input_digest)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.steering_receipts.insert(
            command.steering_id,
            SteeringReceipt {
                input_digest: command.input_digest,
                event: event.clone(),
            },
        );
        Ok(event)
    }

    fn steering_is_duplicate(&self, command: &RunSteeringCommand) -> bool {
        self.accepted
            .get(&command.attempt_id)
            .and_then(|execution| execution.steering_receipts.get(&command.steering_id))
            .is_some_and(|receipt| {
                receipt.input_digest == command.input_digest
                    && receipt.event.attempt_id == command.attempt_id
            })
    }

    pub fn acknowledge_terminal(
        &mut self,
        attempt_id: Uuid,
        event_id: Uuid,
    ) -> Result<(), WorkerAssignmentError> {
        if let Some(completed) = self.completed.get(&attempt_id) {
            return if completed.terminal_event.event_id == event_id {
                Ok(())
            } else {
                Err(WorkerAssignmentError::TerminalEventMismatch)
            };
        }
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let terminal = execution
            .terminal_event
            .as_ref()
            .ok_or(WorkerAssignmentError::TerminalNotReady)?;
        if terminal.event_id != event_id {
            return Err(WorkerAssignmentError::TerminalEventMismatch);
        }
        let run_id = execution.command.run_id;
        let terminal_event = terminal.clone();
        self.accepted.remove(&attempt_id);
        self.completed.insert(
            attempt_id,
            CompletionReceipt {
                run_id,
                terminal_event,
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn heartbeat(&self, occurred_at: DateTime<Utc>) -> WorkerHeartbeat {
        WorkerHeartbeat {
            schema_version: WORKER_HEARTBEAT_SCHEMA_VERSION,
            message_id: Uuid::now_v7(),
            worker_id: self.worker_id,
            incarnation_id: self.worker_incarnation_id,
            occurred_at,
            placements: self.placements.clone(),
            capacity: self.capacity,
            active_runs: self.capacity_consuming_attempts() as u32,
            active_assignments: self
                .accepted
                .values()
                .filter(|execution| {
                    execution.machine.status() != agent_protocol::RunStatus::Suspended
                })
                .map(|execution| ActiveRunAssignment {
                    tenant_id: execution.command.tenant_id,
                    run_id: execution.command.run_id,
                    attempt_id: execution.command.attempt_id,
                    workspace_id: execution.command.workspace_id,
                    owner_epoch: execution.command.owner_epoch,
                    fencing_token: execution.command.fencing_token,
                })
                .collect(),
            runtime_version: self.runtime_version.clone(),
            accepting_work: self.draining.is_none(),
            draining_since: self.draining.map(|drain| drain.started_at),
            drain_deadline: self.draining.map(|drain| drain.deadline),
        }
    }

    fn capacity_consuming_attempts(&self) -> usize {
        self.accepted
            .values()
            .filter(|execution| execution.machine.status() != agent_protocol::RunStatus::Suspended)
            .count()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPollResult {
    Idle,
    Accepted,
    IdentityRenewed,
    Restored,
    Cancelled,
    Steered,
    ApprovalApplied,
    ModelEventPublished,
    ModelExecutionFinished,
    ToolExecutionRequested,
    ToolExecutionStarted,
    ToolResultPublished,
    RetryScheduled,
    Terminated,
}

#[derive(Debug, thiserror::Error)]
#[error("worker transport failed: {0}")]
pub struct WorkerTransportError(String);

pub struct NatsWorker {
    nats_client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    consumer: async_nats::jetstream::consumer::PullConsumer,
    cancellation_consumer: async_nats::jetstream::consumer::PullConsumer,
    steering_consumer: async_nats::jetstream::consumer::PullConsumer,
    approval_consumer: async_nats::jetstream::consumer::PullConsumer,
    recovery_consumer: async_nats::jetstream::consumer::PullConsumer,
    identity_renewal_consumer: async_nats::jetstream::consumer::PullConsumer,
    processor: WorkerProcessor,
    workload_token_verifier: Option<WorkloadTokenVerifier>,
    model_gateway: Option<GrpcModelGatewayClient>,
    checkpoint_store: Option<Arc<dyn CheckpointPayloadStore>>,
    model_supervisor: ModelExecutionSupervisor,
    pending_model_event: Option<PendingModelEvent>,
    pending_model_relaunch: HashSet<Uuid>,
    pending_auth_recovery: HashSet<Uuid>,
    tool_supervisor: ToolExecutionSupervisor,
    tool_executors: HashMap<String, RegisteredToolExecutor>,
    workspace_root: Option<PathBuf>,
    pending_tool_plan: Option<PendingToolPlan>,
    pending_tool_start: Option<PendingToolStart>,
    pending_tool_event: Option<EventEnvelope>,
}

struct RegisteredToolExecutor {
    sandbox: SandboxClass,
    executor: Arc<dyn ToolExecutor>,
}

#[derive(Debug)]
struct PendingModelEvent {
    event: EventEnvelope,
    terminal: bool,
    plan_tool_after_publish: bool,
    hard_budget_exhaustion: Option<BudgetDimension>,
}

#[derive(Clone, Debug)]
struct PendingToolPlan {
    event: EventEnvelope,
    execution: Option<ToolExecutionRequest>,
    followup_event: Option<EventEnvelope>,
}

struct PendingToolStart {
    event: EventEnvelope,
    executor: Arc<dyn ToolExecutor>,
    request: ToolExecutionRequest,
    context: ToolExecutionContext,
}

impl NatsWorker {
    pub async fn connect(
        nats_url: &str,
        processor: WorkerProcessor,
    ) -> Result<Self, WorkerTransportError> {
        Self::connect_internal(nats_url, processor, None).await
    }

    pub async fn connect_with_model_gateway(
        nats_url: &str,
        processor: WorkerProcessor,
        model_gateway_endpoint: &str,
    ) -> Result<Self, WorkerTransportError> {
        let client = GrpcModelGatewayClient::connect(model_gateway_endpoint.to_owned())
            .await
            .map_err(transport_error)?;
        Self::connect_internal(nats_url, processor, Some(client)).await
    }

    pub async fn connect_with_model_gateway_mtls(
        nats_url: &str,
        processor: WorkerProcessor,
        model_gateway_endpoint: &str,
        materials: ClientMtlsMaterials,
    ) -> Result<Self, WorkerTransportError> {
        let client =
            GrpcModelGatewayClient::connect_with_mtls(model_gateway_endpoint.to_owned(), materials)
                .await
                .map_err(transport_error)?;
        Self::connect_internal(nats_url, processor, Some(client)).await
    }

    pub async fn connect_secure_with_model_gateway_mtls(
        nats: &NatsClientConfig,
        processor: WorkerProcessor,
        model_gateway_endpoint: &str,
        materials: ClientMtlsMaterials,
    ) -> Result<Self, WorkerTransportError> {
        let model_gateway =
            GrpcModelGatewayClient::connect_with_mtls(model_gateway_endpoint.to_owned(), materials)
                .await
                .map_err(transport_error)?;
        let nats_client = nats.connect().await.map_err(transport_error)?;
        Self::connect_internal_with_client(nats_client, processor, Some(model_gateway), false).await
    }

    async fn connect_internal(
        nats_url: &str,
        processor: WorkerProcessor,
        model_gateway: Option<GrpcModelGatewayClient>,
    ) -> Result<Self, WorkerTransportError> {
        let client = async_nats::connect(nats_url)
            .await
            .map_err(transport_error)?;
        Self::connect_internal_with_client(client, processor, model_gateway, true).await
    }

    async fn connect_internal_with_client(
        nats_client: async_nats::Client,
        processor: WorkerProcessor,
        model_gateway: Option<GrpcModelGatewayClient>,
        allow_topology_bootstrap: bool,
    ) -> Result<Self, WorkerTransportError> {
        let jetstream = async_nats::jetstream::new(nats_client.clone());
        let execution_stream = if allow_topology_bootstrap {
            jetstream
                .get_or_create_stream(async_nats::jetstream::stream::Config {
                    name: EXECUTION_STREAM_NAME.to_string(),
                    subjects: vec!["runtime.execution.>".to_string()],
                    storage: async_nats::jetstream::stream::StorageType::File,
                    duplicate_window: Duration::from_secs(86_400),
                    ..Default::default()
                })
                .await
                .map_err(transport_error)?
        } else {
            jetstream
                .get_stream(EXECUTION_STREAM_NAME)
                .await
                .map_err(transport_error)?
        };
        if allow_topology_bootstrap {
            jetstream
                .get_or_create_stream(async_nats::jetstream::stream::Config {
                    name: WORKER_EVENT_STREAM_NAME.to_string(),
                    subjects: vec!["runtime.worker.>".to_string()],
                    storage: async_nats::jetstream::stream::StorageType::File,
                    duplicate_window: Duration::from_secs(86_400),
                    ..Default::default()
                })
                .await
                .map_err(transport_error)?;
        } else {
            jetstream
                .get_stream(WORKER_EVENT_STREAM_NAME)
                .await
                .map_err(transport_error)?;
        }

        let worker_id = processor.worker_id();
        let worker_incarnation_id = processor.worker_incarnation_id();
        let durable_name = format!("worker-{worker_id}-{worker_incarnation_id}");
        let subject = execution_subject(worker_id, worker_incarnation_id);
        let consumer = execution_stream
            .get_or_create_consumer(
                &durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: subject,
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        let cancellation_durable_name =
            format!("worker-{worker_id}-{worker_incarnation_id}-cancellations");
        let cancellation_consumer = execution_stream
            .get_or_create_consumer(
                &cancellation_durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(cancellation_durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: cancellation_subject(worker_id, worker_incarnation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        let steering_durable_name = format!("worker-{worker_id}-{worker_incarnation_id}-steering");
        let steering_consumer = execution_stream
            .get_or_create_consumer(
                &steering_durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(steering_durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: steering_subject(worker_id, worker_incarnation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        let approval_durable_name = format!("worker-{worker_id}-{worker_incarnation_id}-approvals");
        let approval_consumer = execution_stream
            .get_or_create_consumer(
                &approval_durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(approval_durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: approval_subject(worker_id, worker_incarnation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        let recovery_durable_name =
            format!("worker-{worker_id}-{worker_incarnation_id}-recoveries");
        let recovery_consumer = execution_stream
            .get_or_create_consumer(
                &recovery_durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(recovery_durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: recovery_subject(worker_id, worker_incarnation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        let identity_durable_name =
            format!("worker-{worker_id}-{worker_incarnation_id}-identity-renewals");
        let identity_renewal_consumer = execution_stream
            .get_or_create_consumer(
                &identity_durable_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(identity_durable_name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: Duration::from_secs(30),
                    max_deliver: 20,
                    filter_subject: identity_renewal_subject(worker_id, worker_incarnation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(transport_error)?;
        Ok(Self {
            nats_client,
            jetstream,
            consumer,
            cancellation_consumer,
            steering_consumer,
            approval_consumer,
            recovery_consumer,
            identity_renewal_consumer,
            processor,
            workload_token_verifier: None,
            model_gateway,
            checkpoint_store: None,
            model_supervisor: ModelExecutionSupervisor::new(256),
            pending_model_event: None,
            pending_model_relaunch: HashSet::new(),
            pending_auth_recovery: HashSet::new(),
            tool_supervisor: ToolExecutionSupervisor::new(256),
            tool_executors: HashMap::new(),
            workspace_root: None,
            pending_tool_plan: None,
            pending_tool_start: None,
            pending_tool_event: None,
        })
    }

    pub fn nats_connection_is_ready(&self) -> bool {
        self.nats_client.connection_state().to_string() == "connected"
    }

    pub fn set_workspace_root(&mut self, workspace_root: PathBuf) {
        self.workspace_root = Some(workspace_root);
    }

    pub fn set_checkpoint_store(&mut self, store: Arc<dyn CheckpointPayloadStore>) {
        self.checkpoint_store = Some(store);
    }

    pub fn set_workload_token_verifier(&mut self, verifier: WorkloadTokenVerifier) {
        self.workload_token_verifier = Some(verifier);
    }

    pub fn register_tool_executor(
        &mut self,
        tool_name: impl Into<String>,
        sandbox: SandboxClass,
        executor: Arc<dyn ToolExecutor>,
    ) -> Result<(), WorkerAssignmentError> {
        let tool_name = tool_name.into();
        if tool_name.trim().is_empty() {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(
                "tool name must not be blank".into(),
            ));
        }
        self.processor.validate_tool_executor(
            &tool_name,
            sandbox,
            executor.implementation_digest(),
        )?;
        if self.tool_executors.contains_key(&tool_name) {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {tool_name} already has an executor"
            )));
        }
        self.tool_executors
            .insert(tool_name, RegisteredToolExecutor { sandbox, executor });
        Ok(())
    }

    pub async fn publish_heartbeat(&self) -> Result<(), WorkerTransportError> {
        let heartbeat = self.processor.heartbeat(Utc::now());
        self.publish_event(WORKER_HEARTBEAT_SUBJECT, heartbeat.message_id, &heartbeat)
            .await
    }

    pub fn begin_draining(
        &mut self,
        started_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<(), WorkerAssignmentError> {
        self.processor.begin_draining(started_at, deadline)
    }

    #[must_use]
    pub fn admission_fence(&self) -> WorkerAdmissionFence {
        self.processor.admission_fence()
    }

    #[must_use]
    pub const fn is_draining(&self) -> bool {
        self.processor.is_draining()
    }

    #[must_use]
    pub fn is_accepting_work(&self) -> bool {
        self.processor.admission_fence.is_open() && !self.processor.is_draining()
    }

    #[must_use]
    pub fn active_attempt_count(&self) -> usize {
        self.processor.active_attempt_ids().len()
    }

    /// Persists the latest safe boundary for every active attempt before a
    /// bounded process shutdown. Running supervisors may finish later; fencing
    /// ensures only the controller-selected replacement can resume a snapshot.
    pub async fn publish_active_checkpoints(&self) -> Result<usize, WorkerTransportError> {
        let mut published = 0;
        for attempt_id in self.processor.checkpointable_attempt_ids() {
            self.publish_checkpoint(attempt_id).await?;
            published += 1;
        }
        Ok(published)
    }

    pub async fn poll_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command = match serde_json::from_slice::<RunExecutionCommand>(&message.payload) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "terminating malformed execution command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        };

        match self.processor.accept(command, Utc::now()) {
            Ok(accepted) => {
                self.publish_event(EXECUTION_ACCEPTED_SUBJECT, accepted.message_id, &accepted)
                    .await?;
                let started = self
                    .processor
                    .start(accepted.attempt_id)
                    .map_err(transport_error)?;
                self.publish_run_event_and_checkpoint(&started).await?;
                let model_launch = if self.model_gateway.is_some() {
                    Some((
                        self.processor
                            .prepare_model_invocation(accepted.attempt_id)
                            .map_err(transport_error)?,
                        self.processor
                            .cancellation_token(accepted.attempt_id)
                            .map_err(transport_error)?,
                    ))
                } else {
                    None
                };
                message.double_ack().await.map_err(transport_error)?;
                if let (Some(client), Some((prepared, cancellation))) =
                    (self.model_gateway.clone(), model_launch)
                {
                    self.model_supervisor.start(
                        accepted.attempt_id,
                        client,
                        prepared,
                        cancellation,
                    );
                }
                Ok(WorkerPollResult::Accepted)
            }
            Err(WorkerAssignmentError::AtCapacity | WorkerAssignmentError::Draining) => {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::RetryScheduled)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected execution command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_identity_renewal_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .identity_renewal_consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command =
            match serde_json::from_slice::<WorkloadIdentityRenewalCommand>(&message.payload) {
                Ok(command) => command,
                Err(error) => {
                    tracing::warn!(%error, "terminating malformed workload identity renewal");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
            };
        let Some(verifier) = self.workload_token_verifier.as_ref() else {
            message
                .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                    Duration::from_secs(1),
                )))
                .await
                .map_err(transport_error)?;
            return Ok(WorkerPollResult::RetryScheduled);
        };
        let attempt_id = command.attempt_id;
        match self
            .processor
            .apply_workload_identity_renewal(command, Utc::now(), verifier)
        {
            Ok(outcome) => {
                message.double_ack().await.map_err(transport_error)?;
                if outcome == WorkloadIdentityRenewalOutcome::Applied
                    && self.pending_auth_recovery.remove(&attempt_id)
                {
                    self.request_model_relaunch(attempt_id)?;
                }
                Ok(WorkerPollResult::IdentityRenewed)
            }
            Err(WorkerAssignmentError::UnknownAttempt) => {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::RetryScheduled)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected workload identity renewal");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_recovery_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .recovery_consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command = match serde_json::from_slice::<RunRecoveryCommand>(&message.payload) {
            Ok(command) if command.validate().is_ok() => command,
            Ok(command) => {
                tracing::warn!(message_id = %command.message_id, "terminating invalid recovery command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
            Err(error) => {
                tracing::warn!(%error, "terminating malformed recovery command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        };
        let recovery_store_context = CheckpointStoreContext {
            tenant_id: command.execution.tenant_id,
            run_id: command.execution.run_id,
            attempt_id: command.execution.attempt_id,
            worker_id: command.execution.worker_id,
            worker_incarnation_id: command.execution.worker_incarnation_id,
            workload_token: command.execution.workload_token.as_str().to_owned(),
        };
        let snapshot = if let Some(payload_ref) = command.checkpoint.payload_ref.as_deref() {
            let Some(store) = self.checkpoint_store.as_ref() else {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::RetryScheduled);
            };
            let stored = match store.get(&recovery_store_context, payload_ref).await {
                Ok(stored) => stored,
                Err(CheckpointStoreError::Corrupt) => {
                    tracing::warn!(%payload_ref, "terminating corrupt checkpoint object recovery");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
                Err(error) => {
                    tracing::warn!(%error, %payload_ref, "checkpoint object is not ready");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                            Duration::from_secs(1),
                        )))
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::RetryScheduled);
                }
            };
            match command.checkpoint.decode_snapshot_with_payload(&stored) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, %payload_ref, "terminating corrupt checkpoint recovery");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
            }
        } else {
            match command.checkpoint.decode_snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "terminating invalid inline checkpoint recovery");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
            }
        };
        let subagent_result = command.subagent_result;
        let steering = command.steering;
        match self
            .processor
            .restore(command.execution, snapshot, Utc::now())
        {
            Ok(restored) => {
                self.publish_event(
                    EXECUTION_ACCEPTED_SUBJECT,
                    restored.accepted.message_id,
                    &restored.accepted,
                )
                .await?;
                self.publish_run_event_and_checkpoint(&restored.event)
                    .await?;
                if let Some(result) = subagent_result.as_ref() {
                    let received = match self
                        .processor
                        .record_subagent_result(restored.accepted.attempt_id, result)
                    {
                        Ok(received) => received,
                        Err(error) => {
                            tracing::warn!(%error, "terminating mismatched subagent recovery result");
                            message
                                .ack_with(async_nats::jetstream::AckKind::Term)
                                .await
                                .map_err(transport_error)?;
                            return Ok(WorkerPollResult::Terminated);
                        }
                    };
                    self.publish_run_event_and_checkpoint(&received).await?;
                }
                if let Some(steering) = steering {
                    let applied = match self.processor.apply_steering(steering, Utc::now()) {
                        Ok(applied) => applied,
                        Err(error) => {
                            tracing::warn!(%error, "terminating mismatched recovery steering");
                            message
                                .ack_with(async_nats::jetstream::AckKind::Term)
                                .await
                                .map_err(transport_error)?;
                            return Ok(WorkerPollResult::Terminated);
                        }
                    };
                    self.publish_run_event_and_checkpoint(&applied).await?;
                }
                let action = self
                    .processor
                    .recovery_action(restored.accepted.attempt_id)
                    .map_err(transport_error)?;
                let resume_result = match action {
                    WorkerRecoveryAction::InvokeModel => {
                        self.request_model_relaunch(restored.accepted.attempt_id)?;
                        WorkerPollResult::Restored
                    }
                    WorkerRecoveryAction::WaitForApproval => {
                        let rebound = self
                            .processor
                            .rebind_recovered_approval(restored.accepted.attempt_id)
                            .map_err(transport_error)?;
                        self.publish_run_event_and_checkpoint(&rebound).await?;
                        WorkerPollResult::Restored
                    }
                    WorkerRecoveryAction::WaitForSubagent => WorkerPollResult::Restored,
                    WorkerRecoveryAction::TerminateBudgetExceeded(_) => {
                        let terminal = self
                            .processor
                            .terminate_pending_budget_exhaustion(restored.accepted.attempt_id)
                            .map_err(transport_error)?
                            .ok_or_else(|| {
                                WorkerTransportError(
                                    "recovered budget exhaustion disappeared".into(),
                                )
                            })?;
                        self.publish_run_event_and_checkpoint(&terminal).await?;
                        self.processor
                            .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                            .map_err(transport_error)?;
                        WorkerPollResult::Restored
                    }
                    WorkerRecoveryAction::PlanPendingTool => {
                        self.prepare_next_tool_plan(restored.accepted.attempt_id)?;
                        self.publish_pending_tool_plan().await?
                    }
                    WorkerRecoveryAction::RetryTool(request) => {
                        let launch = match self
                            .prepare_tool_launch(restored.accepted.attempt_id, request.clone())
                        {
                            Ok(launch) => launch,
                            Err(
                                WorkerAssignmentError::ToolExecutorConfiguration(_)
                                | WorkerAssignmentError::LeaseExpired,
                            ) => {
                                message
                                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                                        Duration::from_secs(1),
                                    )))
                                    .await
                                    .map_err(transport_error)?;
                                return Ok(WorkerPollResult::RetryScheduled);
                            }
                            Err(error) => return Err(transport_error(error)),
                        };
                        let replanned = self
                            .processor
                            .replan_recovered_tool(restored.accepted.attempt_id, &request.call.id)
                            .map_err(transport_error)?;
                        self.publish_run_event_and_checkpoint(&replanned).await?;
                        self.prepare_tool_start(launch.0, launch.1, launch.2)?;
                        self.publish_pending_tool_start().await?
                    }
                };
                if resume_result == WorkerPollResult::RetryScheduled {
                    message
                        .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                            Duration::from_secs(1),
                        )))
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::RetryScheduled);
                }
                message.double_ack().await.map_err(transport_error)?;
                Ok(WorkerPollResult::Restored)
            }
            Err(WorkerAssignmentError::AtCapacity | WorkerAssignmentError::Draining) => {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::RetryScheduled)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected recovery command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_cancellation_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .cancellation_consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command = match serde_json::from_slice::<RunCancellationCommand>(&message.payload) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "terminating malformed cancellation command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        };

        match self.processor.apply_cancellation(command, Utc::now()) {
            Ok(terminal) => {
                self.publish_run_event_and_checkpoint(&terminal).await?;
                self.processor
                    .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                    .map_err(transport_error)?;
                message.double_ack().await.map_err(transport_error)?;
                Ok(WorkerPollResult::Cancelled)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected cancellation command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_steering_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .steering_consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command = match serde_json::from_slice::<RunSteeringCommand>(&message.payload) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "terminating malformed steering command");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        };
        let attempt_id = command.attempt_id;
        if self
            .pending_model_event
            .as_ref()
            .is_some_and(|pending| pending.event.attempt_id == attempt_id)
        {
            message
                .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                    Duration::from_secs(1),
                )))
                .await
                .map_err(transport_error)?;
            return Ok(WorkerPollResult::RetryScheduled);
        }
        let duplicate = self.processor.steering_is_duplicate(&command);
        let outcome_command = command.clone();

        match self.processor.apply_steering(command, Utc::now()) {
            Ok(applied) => {
                self.publish_run_event_and_checkpoint(&applied).await?;
                message.double_ack().await.map_err(transport_error)?;
                if !duplicate {
                    self.request_model_relaunch(attempt_id)?;
                }
                Ok(WorkerPollResult::Steered)
            }
            Err(WorkerAssignmentError::SteeringUnsafe) => {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::RetryScheduled)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected steering command");
                let reason = steering_rejection_reason(&error);
                let outcome = RunSteeringOutcome::rejected(
                    &outcome_command,
                    self.processor.worker_id(),
                    self.processor.worker_incarnation_id(),
                    reason,
                    Utc::now(),
                );
                self.publish_event(RUN_STEERING_OUTCOME_SUBJECT, outcome.message_id, &outcome)
                    .await?;
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_approval_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        let mut messages = self
            .approval_consumer
            .fetch()
            .max_messages(1)
            .expires(timeout)
            .messages()
            .await
            .map_err(transport_error)?;
        let Some(message) = messages.next().await else {
            return Ok(WorkerPollResult::Idle);
        };
        let message = message.map_err(transport_error)?;
        let command = match serde_json::from_slice::<ToolApprovalDecisionCommand>(&message.payload)
        {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "terminating malformed tool approval decision");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        };

        let attempt_id = command.attempt_id;
        let approval_denied = command.decision == ToolApprovalDecision::Deny;
        let tool_launch = if command.decision == ToolApprovalDecision::AllowOnce {
            let request = match self.processor.pending_approval_execution(&command) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(%error, "terminating rejected tool approval decision");
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
            };
            match self.prepare_tool_launch(command.attempt_id, request) {
                Ok(launch) => Some(launch),
                Err(
                    WorkerAssignmentError::ToolExecutorConfiguration(_)
                    | WorkerAssignmentError::LeaseExpired,
                ) => {
                    message
                        .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                            Duration::from_secs(1),
                        )))
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::RetryScheduled);
                }
                Err(error) => return Err(transport_error(error)),
            }
        } else {
            None
        };

        match self.processor.apply_tool_approval(command, Utc::now()) {
            Ok(outcome) => {
                for event in &outcome.events {
                    self.publish_run_event_and_checkpoint(event).await?;
                }
                if let Some((executor, request, context)) = tool_launch {
                    self.prepare_tool_start(executor, request, context)?;
                    self.publish_pending_tool_start().await?;
                }
                message.double_ack().await.map_err(transport_error)?;
                if approval_denied {
                    if self
                        .processor
                        .has_pending_tool_calls(attempt_id)
                        .map_err(transport_error)?
                    {
                        self.prepare_next_tool_plan(attempt_id)?;
                    } else {
                        self.request_model_relaunch(attempt_id)?;
                    }
                }
                Ok(WorkerPollResult::ApprovalApplied)
            }
            Err(error) => {
                tracing::warn!(%error, "terminating rejected tool approval decision");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                Ok(WorkerPollResult::Terminated)
            }
        }
    }

    pub async fn poll_tool_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        if self.pending_tool_start.is_some() {
            return self.publish_pending_tool_start().await;
        }
        if self.pending_tool_plan.is_some() {
            return self.publish_pending_tool_plan().await;
        }
        if self.pending_tool_event.is_some() {
            return self.publish_pending_tool_event().await;
        }
        let Some(update) = self.tool_supervisor.recv(timeout).await else {
            return Ok(WorkerPollResult::Idle);
        };
        let event = match update {
            ToolExecutionUpdate::Finished {
                attempt_id,
                tool_call_id,
                binding_digest,
                result,
            } => self.processor.record_bound_tool_result(
                attempt_id,
                tool_call_id,
                &binding_digest,
                result.content,
                result.is_error,
            ),
            ToolExecutionUpdate::Failed {
                attempt_id,
                tool_call_id,
                binding_digest,
                error,
            } => self.processor.record_bound_tool_result(
                attempt_id,
                tool_call_id,
                &binding_digest,
                serde_json::json!({
                    "error": {
                        "code": tool_execution_error_code(&error),
                        "message": "tool execution failed inside its assigned sandbox"
                    }
                }),
                true,
            ),
        }
        .map_err(transport_error)?;
        self.pending_tool_event = Some(event);
        self.publish_pending_tool_event().await
    }

    async fn publish_pending_tool_event(
        &mut self,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        let event = self
            .pending_tool_event
            .as_ref()
            .expect("pending tool event checked by caller");
        self.publish_run_event_and_checkpoint(event).await?;
        let attempt_id = event.attempt_id;
        self.pending_tool_event = None;
        if self
            .processor
            .has_pending_tool_calls(attempt_id)
            .map_err(transport_error)?
        {
            self.prepare_next_tool_plan(attempt_id)?;
        } else {
            self.request_model_relaunch(attempt_id)?;
        }
        Ok(WorkerPollResult::ToolResultPublished)
    }

    pub async fn poll_model_once(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        if timeout.is_zero() {
            return Err(WorkerTransportError(
                "poll timeout must be positive".to_string(),
            ));
        }
        if self.model_gateway.is_none() {
            return Ok(WorkerPollResult::Idle);
        }
        if self.pending_tool_start.is_some() {
            return self.publish_pending_tool_start().await;
        }
        if self.pending_tool_plan.is_some() {
            return self.publish_pending_tool_plan().await;
        }
        if self.pending_model_event.is_some() {
            return self.publish_pending_model_event().await;
        }
        let Some(update) = self.model_supervisor.recv(timeout).await else {
            return Ok(WorkerPollResult::Idle);
        };
        match update {
            ModelExecutionUpdate::AuthenticationRequired { attempt_id } => {
                if self.pending_model_relaunch.contains(&attempt_id) {
                    return Ok(WorkerPollResult::ModelExecutionFinished);
                }
                self.pending_auth_recovery.insert(attempt_id);
                Ok(WorkerPollResult::RetryScheduled)
            }
            ModelExecutionUpdate::Event { attempt_id, event } => {
                if self.pending_model_relaunch.contains(&attempt_id) {
                    return Ok(WorkerPollResult::ModelExecutionFinished);
                }
                let requested_tool_turn = matches!(
                    event,
                    ModelStreamEvent::Completed {
                        reason: ModelFinishReason::ToolCalls
                    }
                );
                let envelope = match self.processor.apply_model_event(attempt_id, event) {
                    Ok(event) => event,
                    Err(WorkerAssignmentError::AttemptAlreadyTerminal) => {
                        return Ok(WorkerPollResult::ModelExecutionFinished);
                    }
                    Err(error) => return Err(transport_error(error)),
                };
                let terminal = self
                    .processor
                    .attempt_is_terminal(attempt_id)
                    .map_err(transport_error)?;
                let hard_budget_exhaustion = if terminal {
                    None
                } else {
                    self.processor
                        .hard_budget_exhaustion(attempt_id)
                        .map_err(transport_error)?
                };
                self.pending_model_event = Some(PendingModelEvent {
                    event: envelope,
                    terminal,
                    plan_tool_after_publish: requested_tool_turn && !terminal,
                    hard_budget_exhaustion,
                });
                self.publish_pending_model_event().await
            }
            ModelExecutionUpdate::Finished { attempt_id } => {
                if self.pending_model_relaunch.remove(&attempt_id) {
                    self.request_model_relaunch(attempt_id)?;
                }
                Ok(WorkerPollResult::ModelExecutionFinished)
            }
            ModelExecutionUpdate::Cancelled { attempt_id } => {
                self.pending_auth_recovery.remove(&attempt_id);
                if self.pending_model_relaunch.remove(&attempt_id) {
                    self.request_model_relaunch(attempt_id)?;
                }
                Ok(WorkerPollResult::ModelExecutionFinished)
            }
        }
    }

    async fn publish_pending_model_event(
        &mut self,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        let pending = self
            .pending_model_event
            .as_ref()
            .expect("pending model event checked by caller");
        self.publish_run_event_and_checkpoint(&pending.event)
            .await?;
        let attempt_id = pending.event.attempt_id;
        let plan_tool_after_publish = pending.plan_tool_after_publish;
        let hard_budget_exhaustion = pending.hard_budget_exhaustion;
        if pending.terminal {
            self.processor
                .acknowledge_terminal(pending.event.attempt_id, pending.event.event_id)
                .map_err(transport_error)?;
        }
        self.pending_model_event = None;
        if hard_budget_exhaustion.is_some() {
            let terminal = self
                .processor
                .terminate_pending_budget_exhaustion(attempt_id)
                .map_err(transport_error)?
                .ok_or_else(|| {
                    WorkerTransportError("pending budget exhaustion disappeared".into())
                })?;
            self.pending_model_event = Some(PendingModelEvent {
                event: terminal,
                terminal: true,
                plan_tool_after_publish: false,
                hard_budget_exhaustion: None,
            });
            return Ok(WorkerPollResult::ModelEventPublished);
        }
        if plan_tool_after_publish {
            self.prepare_next_tool_plan(attempt_id)?;
            self.publish_pending_tool_plan().await
        } else {
            Ok(WorkerPollResult::ModelEventPublished)
        }
    }

    fn prepare_next_tool_plan(&mut self, attempt_id: Uuid) -> Result<(), WorkerTransportError> {
        let planned = self
            .processor
            .plan_next_tool_call(attempt_id)
            .map_err(transport_error)?;
        let execution = match planned.plan {
            ToolPlan::Execute(request) => Some(request),
            ToolPlan::AutoApproved { execution, .. } => Some(execution),
            ToolPlan::ApprovalRequired(_) | ToolPlan::Denied(_) | ToolPlan::SubagentSpawn(_) => {
                None
            }
        };
        self.pending_tool_plan = Some(PendingToolPlan {
            event: planned.event,
            execution,
            followup_event: planned.followup_event,
        });
        Ok(())
    }

    async fn publish_pending_tool_plan(
        &mut self,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        let pending = self
            .pending_tool_plan
            .as_ref()
            .expect("pending tool plan checked by caller")
            .clone();
        let launch = match pending.execution {
            Some(request) => match self.prepare_tool_launch(pending.event.attempt_id, request) {
                Ok(launch) => Some(launch),
                Err(
                    WorkerAssignmentError::ToolExecutorConfiguration(_)
                    | WorkerAssignmentError::LeaseExpired,
                ) => return Ok(WorkerPollResult::RetryScheduled),
                Err(error) => return Err(transport_error(error)),
            },
            None => None,
        };
        self.publish_run_event_and_checkpoint(&pending.event)
            .await?;
        self.pending_tool_plan = None;
        if let Some((executor, request, context)) = launch {
            self.prepare_tool_start(executor, request, context)?;
            self.publish_pending_tool_start().await
        } else if let Some(followup_event) = pending.followup_event {
            self.pending_tool_event = Some(followup_event);
            self.publish_pending_tool_event().await
        } else {
            Ok(WorkerPollResult::ToolExecutionRequested)
        }
    }

    fn prepare_tool_start(
        &mut self,
        executor: Arc<dyn ToolExecutor>,
        request: ToolExecutionRequest,
        context: ToolExecutionContext,
    ) -> Result<(), WorkerTransportError> {
        let event = self
            .processor
            .record_tool_execution_started(
                context.attempt_id,
                &request.call.id,
                &request.binding_digest,
            )
            .map_err(transport_error)?;
        self.pending_tool_start = Some(PendingToolStart {
            event,
            executor,
            request,
            context,
        });
        Ok(())
    }

    async fn publish_pending_tool_start(
        &mut self,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        let pending = self
            .pending_tool_start
            .as_ref()
            .expect("pending tool start checked by caller");
        self.publish_run_event_and_checkpoint(&pending.event)
            .await?;
        let pending = self
            .pending_tool_start
            .take()
            .expect("published tool start remains pending until PubAck");
        self.tool_supervisor
            .start(pending.executor, pending.request, pending.context);
        Ok(WorkerPollResult::ToolExecutionStarted)
    }

    fn prepare_tool_launch(
        &self,
        attempt_id: Uuid,
        request: ToolExecutionRequest,
    ) -> Result<
        (
            Arc<dyn ToolExecutor>,
            ToolExecutionRequest,
            ToolExecutionContext,
        ),
        WorkerAssignmentError,
    > {
        let registered = self.tool_executors.get(&request.call.name).ok_or_else(|| {
            WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {} has no executor",
                request.call.name
            ))
        })?;
        if registered.sandbox != request.sandbox {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                "tool {} executor sandbox does not match its request",
                request.call.name
            )));
        }
        let workspace_root = self.workspace_root.as_deref().ok_or_else(|| {
            WorkerAssignmentError::ToolExecutorConfiguration(
                "workspace root is not configured".into(),
            )
        })?;
        let context =
            self.processor
                .tool_execution_context(attempt_id, workspace_root, Utc::now())?;
        Ok((registered.executor.clone(), request, context))
    }

    fn request_model_relaunch(&mut self, attempt_id: Uuid) -> Result<(), WorkerTransportError> {
        let Some(client) = self.model_gateway.clone() else {
            return Ok(());
        };
        let prepared = self
            .processor
            .prepare_model_invocation(attempt_id)
            .map_err(transport_error)?;
        let cancellation = self
            .processor
            .cancellation_token(attempt_id)
            .map_err(transport_error)?;
        if !self
            .model_supervisor
            .start(attempt_id, client, prepared, cancellation)
        {
            self.pending_model_relaunch.insert(attempt_id);
        }
        Ok(())
    }

    async fn publish_event<T: serde::Serialize>(
        &self,
        subject: &str,
        message_id: Uuid,
        event: &T,
    ) -> Result<(), WorkerTransportError> {
        let payload = serde_json::to_vec(event).map_err(transport_error)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", message_id.to_string());
        self.jetstream
            .publish_with_headers(subject.to_string(), headers, payload.into())
            .await
            .map_err(transport_error)?
            .await
            .map_err(transport_error)?;
        Ok(())
    }

    async fn publish_run_event_and_checkpoint(
        &self,
        event: &EventEnvelope,
    ) -> Result<(), WorkerTransportError> {
        if self
            .processor
            .attempt_is_terminal(event.attempt_id)
            .map_err(transport_error)?
        {
            return self
                .publish_event(RUN_EVENT_SUBJECT, event.event_id, event)
                .await;
        }
        let prepared = self.prepare_and_store_checkpoint(event.attempt_id).await?;
        self.publish_event(RUN_EVENT_SUBJECT, event.event_id, event)
            .await?;
        self.publish_event(
            CHECKPOINT_SUBJECT,
            prepared.message.message_id,
            &prepared.message,
        )
        .await
    }

    async fn publish_checkpoint(&self, attempt_id: Uuid) -> Result<(), WorkerTransportError> {
        let prepared = self.prepare_and_store_checkpoint(attempt_id).await?;
        self.publish_event(
            CHECKPOINT_SUBJECT,
            prepared.message.message_id,
            &prepared.message,
        )
        .await
    }

    async fn prepare_and_store_checkpoint(
        &self,
        attempt_id: Uuid,
    ) -> Result<PreparedRunCheckpoint, WorkerTransportError> {
        let prepared = self
            .processor
            .prepare_checkpoint_message(attempt_id, Utc::now())
            .map_err(transport_error)?;
        if let Some(stored) = prepared.external_payload.as_deref() {
            let context = self
                .processor
                .checkpoint_store_context(attempt_id)
                .map_err(transport_error)?;
            let payload_ref = prepared.message.payload_ref.as_deref().ok_or_else(|| {
                WorkerTransportError("external checkpoint is missing its object reference".into())
            })?;
            let store = self.checkpoint_store.as_ref().ok_or_else(|| {
                WorkerTransportError(
                    "external checkpoint requires a checkpoint payload store".into(),
                )
            })?;
            store
                .put(&context, payload_ref, stored)
                .await
                .map_err(transport_error)?;
        }
        Ok(prepared)
    }
}

#[must_use]
pub fn execution_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.run.v2")
}

#[must_use]
pub fn cancellation_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.cancel.v2")
}

#[must_use]
pub fn steering_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.steer.v1")
}

fn steering_rejection_reason(error: &WorkerAssignmentError) -> &'static str {
    match error {
        WorkerAssignmentError::SteeringExpired => "expired",
        WorkerAssignmentError::WrongWorker => "wrong_worker",
        WorkerAssignmentError::WrongWorkerIncarnation => "wrong_worker_incarnation",
        WorkerAssignmentError::UnknownAttempt => "unknown_attempt",
        WorkerAssignmentError::AttemptConflict => "attempt_conflict",
        WorkerAssignmentError::LeaseExpired => "lease_expired",
        WorkerAssignmentError::AttemptAlreadyTerminal => "attempt_terminal",
        WorkerAssignmentError::SteeringConflict => "conflicting_replay",
        WorkerAssignmentError::InvalidSteering(_) => "invalid_command",
        _ => "worker_rejected",
    }
}

#[must_use]
pub fn recovery_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.restore.v2")
}

#[must_use]
pub fn approval_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.approval.v2")
}

#[must_use]
pub fn identity_renewal_subject(worker_id: Uuid, worker_incarnation_id: Uuid) -> String {
    format!("runtime.execution.worker.{worker_id}.incarnation.{worker_incarnation_id}.identity.v1")
}

fn transport_error(error: impl std::fmt::Display) -> WorkerTransportError {
    WorkerTransportError(error.to_string())
}

fn tool_execution_error_code(error: &ToolExecutionError) -> &'static str {
    match error {
        ToolExecutionError::TimedOut => "tool_timeout",
        ToolExecutionError::Cancelled => "tool_cancelled",
        ToolExecutionError::OutputLimitExceeded => "tool_output_limit_exceeded",
        ToolExecutionError::OutputBindingMismatch => "tool_output_binding_mismatch",
        ToolExecutionError::WrongSandbox => "tool_wrong_sandbox",
        // Kept distinct from tool_execution_failed on purpose: this is a
        // containment posture failure, not a Tool that misbehaved. An operator
        // seeing it needs to know the sandbox could not be established.
        ToolExecutionError::ContainmentUnavailable(_) => "tool_containment_unavailable",
        ToolExecutionError::InvalidDefinition(_)
        | ToolExecutionError::InvalidContext(_)
        | ToolExecutionError::Engine(_)
        | ToolExecutionError::ProcessFailed { .. }
        | ToolExecutionError::InvalidOutput(_)
        | ToolExecutionError::ExecutableChanged => "tool_execution_failed",
    }
}
