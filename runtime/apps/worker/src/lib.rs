use agent_grpc_security::ClientMtlsMaterials;
use agent_kernel::{RunCommand, RunMachine, SubagentInputAcceptance, ToolPlan, ToolRegistry};
use agent_model_gateway_protocol::v1::content_part;
use agent_model_gateway_protocol::v1::{
    ContentPart, ModelInvocation, ModelMessage, ModelRole, ModelTool,
    ProviderPrivateState as WireProviderPrivateState, ReasoningPart, ReasoningPolicy, RefusalPart,
    TextPart, ToolCallPart, ToolResultPart,
};
use agent_nats_security::NatsClientConfig;
use agent_protocol::{
    ActiveRunAssignment, ApprovalMode, BudgetDimension, EventEnvelope, HistoryRepairReport,
    McpClientCapability, McpElicitationRequest, McpInputContinuation, McpInputRequired,
    McpInputResolutionCommand, McpProtocolRevision, McpServerCapability, ModelErrorKind,
    ModelFinishReason, ModelStreamEvent, Placement, PreparedRunCheckpoint,
    RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION, RUN_EXECUTION_COMPLETE_IDENTITY_SCHEMA_VERSION,
    RunCancellationCommand, RunCheckpointPublished, RunExecutionAccepted, RunExecutionCommand,
    RunRecoveryCommand, RunStatus, RunSteeringCommand, RunSteeringOutcome,
    RuntimeExecutionPolicySnapshot, SandboxClass, SubagentConversationTurn, SubagentForkReceipt,
    SubagentResultDelivery, SubagentRole, SubagentRollbackReceipt, SubagentSpawnMode,
    SubagentSpawnRequest, ToolApprovalDecision, ToolApprovalDecisionCommand, ToolCall,
    ToolDescriptor, ToolEffect, ToolExecutionRequest, WORKER_HEARTBEAT_SCHEMA_VERSION,
    WorkerHeartbeat, WorkloadIdentityRenewalCommand, WorkloadToken, repair_imported_history,
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
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod checkpoint_gateway;
mod execution_supervisor;
mod mcp_discovery_coordinator;
mod mcp_discovery_supervisor;
mod mcp_gateway;
mod model_gateway;
mod tool_execution_supervisor;

pub use checkpoint_gateway::GrpcCheckpointPayloadStore;
pub use execution_supervisor::{ModelExecutionSupervisor, ModelExecutionUpdate};
pub use mcp_discovery_coordinator::{McpDiscoveryCompletion, McpDiscoveryCoordinator};
pub use mcp_discovery_supervisor::{McpDiscoverySupervisor, McpDiscoveryUpdate};
pub use mcp_gateway::{
    DiscoveredCatalog, DiscoveredTool, FederatedRunTools, FederatedToolExecutor,
    FederationIdentity, GET_MCP_PROMPT_TOOL, GrpcMcpFederationClient, LIST_MCP_PROMPTS_TOOL,
    LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, McpCallContext, McpDiscoveryPolicy,
    McpDiscoveryScheduler, McpDiscoverySchedulerSnapshot, McpFederationBackend,
    McpFederationClient, McpGatewayClientError, McpProgressNotification, McpServerDiscoveryStatus,
    McpServerHealth, McpToolRoundOutcome, READ_MCP_RESOURCE_TOOL,
    attach_discovered_federated_tools, discover_federated_tools,
    discover_federated_tools_with_policy, is_runtime_mcp_read_tool,
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
pub const WORKER_CHECKPOINT_SCHEMA_VERSION: u32 = 26;
const SUBAGENT_MAX_GENERATIONS: u64 = 32;
const SUBAGENT_ARCHIVE_MAX_TURNS: usize = 512;
const SUBAGENT_ARCHIVE_MAX_BYTES: usize = 8 * 1024 * 1024;
const COMPACTION_SUMMARY_MAX_BYTES: usize = 262_144;
const COMPACTION_SUMMARY_PREFIX: &str = "[Earlier conversation summary]";
const COMPACTION_SYSTEM_PROMPT: &str = "Summarize the supplied older conversation prefix for a later model turn. Preserve goals, decisions, unresolved work, file or resource identities, failures, permissions, budgets, and Tool outcomes. Do not invent facts. Return only the summary.";
const COMPACTION_FINAL_INSTRUCTION: &str =
    "Produce the bounded continuation summary now. Recent messages are retained separately.";

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
    pub application_id: Uuid,
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
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

fn model_messages_digest(messages: &[ModelMessage]) -> String {
    let mut hasher = Sha256::new();
    for message in messages {
        let encoded = message.encode_to_vec();
        hasher.update(
            u64::try_from(encoded.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(encoded);
    }
    hex::encode(hasher.finalize())
}

fn model_messages_size(messages: &[ModelMessage]) -> u64 {
    messages.iter().fold(0, |total, message| {
        total.saturating_add(u64::try_from(message.encoded_len()).unwrap_or(u64::MAX))
    })
}

fn message_tool_call_ids(message: &ModelMessage) -> BTreeSet<&str> {
    message
        .content
        .iter()
        .filter_map(|part| match part.body.as_ref() {
            Some(content_part::Body::ToolCall(call)) => Some(call.tool_call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn message_tool_result_ids(message: &ModelMessage) -> BTreeSet<&str> {
    message
        .content
        .iter()
        .filter_map(|part| match part.body.as_ref() {
            Some(content_part::Body::ToolResult(result)) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn transcript_has_complete_tool_pairs(messages: &[ModelMessage]) -> bool {
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();
    for message in messages {
        for call_id in message_tool_call_ids(message) {
            if call_id.trim().is_empty() || !calls.insert(call_id) {
                return false;
            }
        }
        for result_id in message_tool_result_ids(message) {
            if result_id.trim().is_empty() || !results.insert(result_id) {
                return false;
            }
        }
    }
    calls == results
}

fn protocol_message_from_model(
    message: &ModelMessage,
) -> Result<Option<agent_protocol::Message>, WorkerAssignmentError> {
    let role = match ModelRole::try_from(message.role).ok() {
        Some(ModelRole::System) => return Ok(None),
        Some(ModelRole::User) => agent_protocol::Role::User,
        Some(ModelRole::Assistant) => agent_protocol::Role::Assistant,
        Some(ModelRole::Tool) => agent_protocol::Role::Tool,
        Some(ModelRole::Unspecified) | None => {
            return Err(WorkerAssignmentError::InvalidTranscript(
                "model message has an unspecified role".into(),
            ));
        }
    };
    let content = message
        .content
        .iter()
        .map(|part| match part.body.as_ref() {
            Some(content_part::Body::Text(part)) => Ok(agent_protocol::ContentPart::Text {
                text: part.text.clone(),
            }),
            Some(content_part::Body::Image(part)) => Ok(agent_protocol::ContentPart::Image {
                media_type: part.media_type.clone(),
                source: part.source.clone(),
            }),
            Some(content_part::Body::Audio(part)) => Ok(agent_protocol::ContentPart::Audio {
                media_type: part.media_type.clone(),
                source: part.source.clone(),
            }),
            Some(content_part::Body::ToolResult(part)) => {
                let content = serde_json::from_slice(&part.content_json).map_err(|error| {
                    WorkerAssignmentError::InvalidTranscript(format!(
                        "Tool result content is not JSON: {error}"
                    ))
                })?;
                Ok(agent_protocol::ContentPart::ToolResult {
                    tool_call_id: part.tool_call_id.clone(),
                    content,
                })
            }
            Some(content_part::Body::ToolCall(part)) => {
                let arguments = serde_json::from_slice(&part.arguments_json).map_err(|error| {
                    WorkerAssignmentError::InvalidTranscript(format!(
                        "Tool call arguments are not JSON: {error}"
                    ))
                })?;
                Ok(agent_protocol::ContentPart::ToolCall {
                    tool_call_id: part.tool_call_id.clone(),
                    name: part.name.clone(),
                    arguments,
                })
            }
            Some(content_part::Body::Reasoning(part)) => {
                let private_state =
                    part.private_state
                        .as_ref()
                        .map(|state| agent_protocol::ProviderPrivateState {
                            provider_id: state.provider_id.clone(),
                            protocol: state.protocol.clone(),
                            model: state.model.clone(),
                            format: state.format.clone(),
                            data: state.data.clone(),
                        });
                if private_state
                    .as_ref()
                    .is_some_and(|state| !state.is_well_formed())
                {
                    return Err(WorkerAssignmentError::InvalidTranscript(
                        "provider-private model state is malformed".into(),
                    ));
                }
                Ok(agent_protocol::ContentPart::Reasoning {
                    summary: part.summary.clone(),
                    private_state,
                })
            }
            Some(content_part::Body::Refusal(part)) => Ok(agent_protocol::ContentPart::Refusal {
                text: part.text.clone(),
            }),
            None => Err(WorkerAssignmentError::InvalidTranscript(
                "model message has an empty content part".into(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(agent_protocol::Message { role, content }))
}

fn model_message_from_protocol(message: &agent_protocol::Message) -> ModelMessage {
    let role = match message.role {
        agent_protocol::Role::System => ModelRole::System,
        agent_protocol::Role::User => ModelRole::User,
        agent_protocol::Role::Assistant => ModelRole::Assistant,
        agent_protocol::Role::Tool => ModelRole::Tool,
    };
    let content = message
        .content
        .iter()
        .map(|part| ContentPart {
            body: Some(match part {
                agent_protocol::ContentPart::Text { text } => {
                    content_part::Body::Text(TextPart { text: text.clone() })
                }
                agent_protocol::ContentPart::Image { media_type, source } => {
                    content_part::Body::Image(agent_model_gateway_protocol::v1::MediaPart {
                        media_type: media_type.clone(),
                        source: source.clone(),
                    })
                }
                agent_protocol::ContentPart::Audio { media_type, source } => {
                    content_part::Body::Audio(agent_model_gateway_protocol::v1::MediaPart {
                        media_type: media_type.clone(),
                        source: source.clone(),
                    })
                }
                agent_protocol::ContentPart::ToolResult {
                    tool_call_id,
                    content,
                } => content_part::Body::ToolResult(ToolResultPart {
                    tool_call_id: tool_call_id.clone(),
                    content_json: serde_json::to_vec(content)
                        .expect("validated Tool result content is serializable"),
                }),
                agent_protocol::ContentPart::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                } => content_part::Body::ToolCall(ToolCallPart {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments_json: serde_json::to_vec(arguments)
                        .expect("validated Tool call arguments are serializable"),
                }),
                agent_protocol::ContentPart::Reasoning {
                    summary,
                    private_state,
                } => content_part::Body::Reasoning(ReasoningPart {
                    summary: summary.clone(),
                    private_state: private_state
                        .as_ref()
                        .map(|state| WireProviderPrivateState {
                            provider_id: state.provider_id.clone(),
                            protocol: state.protocol.clone(),
                            model: state.model.clone(),
                            format: state.format.clone(),
                            data: state.data.clone(),
                        }),
                }),
                agent_protocol::ContentPart::Refusal { text } => {
                    content_part::Body::Refusal(RefusalPart { text: text.clone() })
                }
            }),
        })
        .collect();
    ModelMessage {
        role: role as i32,
        content,
    }
}

fn compaction_binding_digest(
    command: &RunExecutionCommand,
    source_transcript_digest: &str,
    source_prefix_digest: &str,
    source_message_count: u32,
    retained_tail_digest: &str,
    retained_message_count: u32,
) -> String {
    digest_bytes(
        &serde_json::to_vec(&(
            "agent-runtime-context-compaction-v1",
            command.tenant_id,
            command.run_id,
            command.session_id,
            command.model_policy_id,
            source_transcript_digest,
            source_prefix_digest,
            source_message_count,
            retained_tail_digest,
            retained_message_count,
            command
                .runtime_policy
                .as_ref()
                .map(|policy| &policy.context_compaction),
        ))
        .expect("context compaction binding is serializable"),
    )
}

fn subagent_turn_assistant_text(turn: &SubagentConversationTurn) -> String {
    if turn.result.terminal_status == agent_protocol::RunStatus::Succeeded
        && let Some(text) = turn
            .result
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
    {
        return text.to_owned();
    }
    serde_json::to_string(&serde_json::json!({
        "terminal_status": turn.result.terminal_status,
        "is_error": turn.result.is_error,
        "content": &turn.result.content,
    }))
    .expect("subagent history result is serializable")
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
/// The server namespace of a qualified federated tool name, if it is one.
///
/// One place, so the parse used to admit a name and the parse used to route it
/// cannot drift apart.
fn federated_server_of(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("mcp:")
        .and_then(|rest| rest.split_once('/'))
        .map(|(server, _)| server)
        .filter(|server| !server.is_empty())
}

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

/// Binds the remote authority behind every federated namespace. A catalog
/// digest alone is not an identity: another endpoint can deliberately publish
/// the same Tool schema and would otherwise inherit an existing Run's approval
/// and transcript after recovery.
fn mcp_server_binding_digest(
    execution_schema_version: u32,
    servers: &[agent_protocol::McpServerSnapshot],
) -> String {
    let bindings = servers
        .iter()
        .map(|server| {
            let mut binding = serde_json::json!({
                "server_id": server.server_id,
                "name": server.name,
                "endpoint": server.endpoint,
                "credential_envelope_digest": digest_bytes(
                    server.credential_envelope_base64.as_bytes()
                ),
            });
            if execution_schema_version >= 11 {
                binding["required"] = serde_json::json!(server.required);
            }
            if execution_schema_version >= 18 {
                binding["tool_effect_overrides"] = serde_json::json!(server.tool_effect_overrides);
            }
            if execution_schema_version >= 19 {
                binding["protocol_revision"] = serde_json::json!(server.protocol_revision);
                binding["client_capabilities"] = serde_json::json!(server.client_capabilities);
            }
            binding
        })
        .collect::<Vec<_>>();
    digest_bytes(&serde_json::to_vec(&bindings).expect("MCP server bindings are serializable"))
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
    history_repair: Option<HistoryRepairReport>,
    /// Text emitted in the current assistant turn. It is committed to the
    /// transcript together with that turn's Tool calls, so a follow-up model
    /// request never sees a call stripped of its preceding explanation.
    assistant_text_buffer: String,
    /// Non-display assistant items (reasoning continuation state and refusal)
    /// accumulated until the provider completes this turn.
    assistant_rich_content_buffer: Vec<ContentPart>,
    pending_transcript_compaction: Option<PendingTranscriptCompaction>,
    transcript_compaction: Option<TranscriptCompactionRecord>,
    effective_agent_instructions: String,
    effective_tool_names: BTreeSet<String>,
    effective_tool_catalog_digest: String,
    effective_skill_binding_digest: String,
    pending_tool_calls: VecDeque<ToolCall>,
    outstanding_tool_calls: HashMap<String, ToolExecutionRequest>,
    started_tool_calls: HashMap<String, EventEnvelope>,
    ordered_tool_commit_queue: VecDeque<String>,
    staged_ordered_tool_results: BTreeMap<String, StagedOrderedToolResult>,
    recovery_replanned_tools: HashMap<String, EventEnvelope>,
    rebound_approval_event: Option<EventEnvelope>,
    pending_approval: Option<agent_protocol::ToolApprovalRequest>,
    pending_mcp_input: Option<McpInputRequired>,
    resolved_mcp_input: Option<ResolvedMcpInput>,
    pending_subagents: Vec<SubagentSpawnRequest>,
    active_subagents: BTreeMap<Uuid, SubagentSpawnRequest>,
    completed_subagents: BTreeMap<Uuid, SubagentResultDelivery>,
    subagent_handles: BTreeMap<Uuid, SubagentSpawnRequest>,
    subagent_message_sequences: BTreeMap<Uuid, u64>,
    subagent_message_receipts: BTreeMap<Uuid, BTreeMap<String, SubagentMessageReceipt>>,
    subagent_message_queues: BTreeMap<Uuid, VecDeque<String>>,
    subagent_conversations: BTreeMap<Uuid, Vec<SubagentConversationTurn>>,
    subagent_activation_sequences: BTreeMap<Uuid, u64>,
    subagent_generations: BTreeMap<Uuid, u64>,
    subagent_fork_receipts: BTreeMap<String, SubagentForkRecord>,
    subagent_archived_turns: BTreeMap<Uuid, BTreeMap<u64, SubagentConversationTurn>>,
    subagent_generation_heads: BTreeMap<Uuid, BTreeMap<u64, SubagentGenerationHead>>,
    subagent_rollback_receipts: BTreeMap<String, SubagentRollbackRecord>,
    subagent_budget_reservations: BTreeMap<Uuid, SubagentBudgetReservation>,
    closed_subagents: BTreeSet<Uuid>,
    /// Idempotency receipts are per Tool call. A single Option made the first
    /// completed child permanently block every later serial child in the same
    /// attempt, even though their bindings were independent.
    subagent_result_receipts: HashMap<String, (String, EventEnvelope)>,
    steering_receipts: HashMap<Uuid, SteeringReceipt>,
    budget_usage: BudgetUsage,
    execution_time: ExecutionTimeBudget,
    /// Federated Tools discovered for this Run, and the executors that call
    /// them (ADR-0040).
    ///
    /// Per attempt rather than per Worker because a frozen catalog belongs to a
    /// Run: two Runs against one server can hold different digests, and a
    /// Worker-wide map could only hold one of them.
    federated_registry: Option<ToolRegistry>,
    /// Not part of `Debug`: a trait object has none, and a Run's diagnostics
    /// should not start depending on what an executor prints anyway.
    federated_executors: FederatedExecutors,
    /// Their definitions, so the model can be offered them. Without this the
    /// Run holds tools it can execute and the model never learns they exist.
    federated_definitions: Vec<WorkerToolDefinition>,
    /// Qualified Tool name -> the catalog digest that defined it.
    ///
    /// This is persisted independently from the native Tool catalog. Federated
    /// definitions are discovered per Run and therefore are not present in the
    /// Worker's global `tool_definitions` map used by the native digest.
    federated_tool_bindings: BTreeMap<String, String>,
    /// Present only after restoring a schema-6+ checkpoint. Re-discovery must
    /// reproduce this exact set before any recovered Tool can continue.
    expected_federated_tool_bindings: Option<BTreeMap<String, String>>,
    federated_discovery_policy: Option<McpDiscoveryPolicy>,
    /// Present only after restoring a schema-7+ MCP checkpoint.
    expected_federated_discovery_policy: Option<McpDiscoveryPolicy>,
    /// Server-scoped Resources/Prompts authority rebuilt from discovery for
    /// Runtime-owned read Tools (ADR-0117). This is never inferred from remote
    /// Tool grants and is empty until the exact frozen directory is attached.
    runtime_mcp_read_servers: BTreeMap<String, mcp_gateway::RuntimeMcpReadServerBinding>,
    pending_budget_exhaustion: Option<PendingBudgetExhaustion>,
    approval_decisions: HashMap<Uuid, ApprovalDecisionReceipt>,
    applied_approval_decisions: HashMap<Uuid, AppliedApprovalDecision>,
    restored_from_checkpoint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SteeringReceipt {
    input_digest: String,
    event: EventEnvelope,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SubagentForkRecord {
    receipt: SubagentForkReceipt,
    event: EventEnvelope,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SubagentGenerationHead {
    activation_ordinals: Vec<u64>,
    history_digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SubagentRollbackRecord {
    receipt: SubagentRollbackReceipt,
    event: EventEnvelope,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SubagentBudgetReservation {
    agent_id: Uuid,
    tool_call_id: String,
    binding_digest: String,
    budget: agent_protocol::RunBudget,
}

impl SubagentBudgetReservation {
    fn new(agent_id: Uuid, request: &SubagentSpawnRequest) -> Self {
        Self {
            agent_id,
            tool_call_id: request.tool_call_id.clone(),
            binding_digest: request.binding_digest.clone(),
            budget: request.budget.clone(),
        }
    }

    fn is_well_formed(&self) -> bool {
        !self.agent_id.is_nil()
            && !self.tool_call_id.trim().is_empty()
            && self.binding_digest.len() == 64
            && self
                .binding_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && self.budget.max_tokens > 0
            && self.budget.max_cost_cents > 0
            && (1..=86_400).contains(&self.budget.max_duration_seconds)
    }
}

fn insert_subagent_budget_reservation(
    reservations: &mut BTreeMap<Uuid, SubagentBudgetReservation>,
    agent_id: Uuid,
    request: &SubagentSpawnRequest,
) -> Result<(), WorkerAssignmentError> {
    let reservation = SubagentBudgetReservation::new(agent_id, request);
    if !reservation.is_well_formed() || reservations.contains_key(&request.delegation_id) {
        return Err(WorkerAssignmentError::InvalidToolCall);
    }
    reservations.insert(request.delegation_id, reservation);
    Ok(())
}

fn remove_subagent_budget_reservation(
    reservations: &mut BTreeMap<Uuid, SubagentBudgetReservation>,
    agent_id: Uuid,
    request: &SubagentSpawnRequest,
) -> Result<(), WorkerAssignmentError> {
    let expected = SubagentBudgetReservation::new(agent_id, request);
    if reservations.get(&request.delegation_id) != Some(&expected) {
        return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
    }
    reservations.remove(&request.delegation_id);
    Ok(())
}

fn rebind_subagent_budget_reservation(
    reservations: &mut BTreeMap<Uuid, SubagentBudgetReservation>,
    agent_id: Uuid,
    previous: &SubagentSpawnRequest,
    activated: &SubagentSpawnRequest,
) -> Result<(), WorkerAssignmentError> {
    if previous.delegation_id != activated.delegation_id
        || previous.budget != activated.budget
        || reservations.get(&previous.delegation_id)
            != Some(&SubagentBudgetReservation::new(agent_id, previous))
    {
        return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
    }
    reservations.insert(
        activated.delegation_id,
        SubagentBudgetReservation::new(agent_id, activated),
    );
    Ok(())
}

fn subagent_budget_reservation_totals(
    reservations: &BTreeMap<Uuid, SubagentBudgetReservation>,
) -> agent_protocol::RunBudget {
    reservations.values().fold(
        agent_protocol::RunBudget {
            max_tokens: 0,
            max_cost_cents: 0,
            max_duration_seconds: 0,
        },
        |mut total, reservation| {
            total.max_tokens = total
                .max_tokens
                .saturating_add(reservation.budget.max_tokens);
            total.max_cost_cents = total
                .max_cost_cents
                .saturating_add(reservation.budget.max_cost_cents);
            total.max_duration_seconds = total
                .max_duration_seconds
                .saturating_add(reservation.budget.max_duration_seconds);
            total
        },
    )
}

fn rebuild_subagent_budget_reservations(
    pending: &[SubagentSpawnRequest],
    active: &BTreeMap<Uuid, SubagentSpawnRequest>,
    message_receipts: &BTreeMap<Uuid, BTreeMap<String, SubagentMessageReceipt>>,
) -> Result<BTreeMap<Uuid, SubagentBudgetReservation>, WorkerAssignmentError> {
    let mut reservations = BTreeMap::new();
    for request in pending {
        insert_subagent_budget_reservation(&mut reservations, request.delegation_id, request)
            .map_err(|_| {
                WorkerAssignmentError::InvalidCheckpoint(
                    "pending subagent budget reservation is malformed".into(),
                )
            })?;
    }
    for (agent_id, request) in active {
        insert_subagent_budget_reservation(&mut reservations, *agent_id, request).map_err(
            |_| {
                WorkerAssignmentError::InvalidCheckpoint(
                    "active subagent budget reservation is malformed".into(),
                )
            },
        )?;
    }
    for (agent_id, receipts) in message_receipts {
        for receipt in receipts
            .values()
            .filter(|receipt| receipt.status == SubagentMessageStatus::Queued)
        {
            insert_subagent_budget_reservation(
                &mut reservations,
                *agent_id,
                &receipt.child_request,
            )
            .map_err(|_| {
                WorkerAssignmentError::InvalidCheckpoint(
                    "queued subagent budget reservation is malformed".into(),
                )
            })?;
        }
    }
    Ok(reservations)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct BudgetUsage {
    tokens: u64,
    cost_micros: u64,
}

#[derive(Debug)]
struct ExecutionTimeBudget {
    accumulated_millis: u64,
    active_since: Option<Instant>,
}

impl ExecutionTimeBudget {
    fn new() -> Self {
        Self {
            accumulated_millis: 0,
            active_since: Some(Instant::now()),
        }
    }

    fn restore(state: Option<CheckpointExecutionTime>, restored_at: DateTime<Utc>) -> Self {
        let accumulated_millis = state.map_or(0, |state| {
            let recovery_gap = if state.active {
                let gap = restored_at
                    .signed_duration_since(state.checkpointed_at)
                    .num_milliseconds();
                if gap < 0 {
                    // A monotonic clock cannot cross process restarts. UTC is
                    // only the recovery bridge; rollback is therefore
                    // suspicious and fails closed instead of granting time.
                    u64::MAX
                } else {
                    gap as u64
                }
            } else {
                0
            };
            state.elapsed_millis.saturating_add(recovery_gap)
        });
        Self {
            accumulated_millis,
            active_since: Some(Instant::now()),
        }
    }

    fn elapsed_millis(&self) -> u64 {
        self.active_since
            .map_or(self.accumulated_millis, |started| {
                self.accumulated_millis.saturating_add(
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                )
            })
    }

    fn remaining(&self, max_duration_seconds: u64) -> Duration {
        Duration::from_secs(max_duration_seconds)
            .saturating_sub(Duration::from_millis(self.elapsed_millis()))
    }

    fn pause(&mut self) {
        if let Some(started) = self.active_since.take() {
            self.accumulated_millis = self
                .accumulated_millis
                .saturating_add(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
    }

    fn resume(&mut self) {
        if self.active_since.is_none() {
            self.active_since = Some(Instant::now());
        }
    }

    fn is_active(&self) -> bool {
        self.active_since.is_some()
    }

    fn checkpoint(&self, checkpointed_at: DateTime<Utc>) -> CheckpointExecutionTime {
        CheckpointExecutionTime {
            elapsed_millis: self.elapsed_millis(),
            checkpointed_at,
            active: self.active_since.is_some(),
        }
    }
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
    #[serde(default)]
    mode: SubagentSpawnMode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentWaitArguments {
    agent_id: Uuid,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentCloseArguments {
    agent_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentSendArguments {
    agent_id: Uuid,
    #[serde(default)]
    generation: Option<u64>,
    message: String,
    idempotency_key: String,
    #[serde(default)]
    interrupt: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentHistoryArguments {
    agent_id: Uuid,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    after_activation_ordinal: Option<u64>,
    limit: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentForkArguments {
    source_agent_id: Uuid,
    source_generation: u64,
    through_activation_ordinal: u64,
    max_tokens: u64,
    max_cost_cents: u64,
    max_duration_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentRollbackArguments {
    agent_id: Uuid,
    generation: u64,
    through_activation_ordinal: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SubagentHistoryPage {
    pub agent_id: Uuid,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<SubagentForkReceipt>,
    pub turns: Vec<SubagentConversationTurn>,
    pub next_after_activation_ordinal: Option<u64>,
    pub has_more: bool,
    pub status: String,
    pub queued_messages: usize,
    pub closed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMessageStatus {
    Queued,
    Active,
    Completed,
    Cancelled,
}

fn default_subagent_message_status() -> SubagentMessageStatus {
    SubagentMessageStatus::Active
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentMessageReceipt {
    pub agent_id: Uuid,
    pub idempotency_key: String,
    pub message_digest: String,
    pub message_sequence: u64,
    pub submission_id: String,
    #[serde(default)]
    pub interrupt: bool,
    #[serde(default = "default_subagent_message_status")]
    pub status: SubagentMessageStatus,
    pub child_request: SubagentSpawnRequest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AsyncSubagentContinuation {
    pub receipt: SubagentMessageReceipt,
    pub accepted_event: Option<EventEnvelope>,
    /// Present only while this receipt still owns an unfinished child Run.
    /// Launching is idempotent in the Host task cache.
    pub active_request: Option<SubagentSpawnRequest>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentForkOutcome {
    pub receipt: SubagentForkReceipt,
    pub event: EventEnvelope,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentRollbackOutcome {
    pub receipt: SubagentRollbackReceipt,
    pub event: EventEnvelope,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentMessageActivation {
    pub receipt: SubagentMessageReceipt,
    pub event: EventEnvelope,
    pub request: SubagentSpawnRequest,
}

fn valid_subagent_message_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn materialize_generation_from_parts(
    current_generation: u64,
    current: &[SubagentConversationTurn],
    archived: Option<&BTreeMap<u64, SubagentConversationTurn>>,
    heads: Option<&BTreeMap<u64, SubagentGenerationHead>>,
    generation: u64,
) -> Option<Vec<SubagentConversationTurn>> {
    if generation == current_generation {
        return Some(current.to_vec());
    }
    let head = heads?.get(&generation)?;
    let current_by_ordinal = current
        .iter()
        .map(|turn| (turn.activation_ordinal, turn))
        .collect::<BTreeMap<_, _>>();
    head.activation_ordinals
        .iter()
        .map(|ordinal| {
            current_by_ordinal
                .get(ordinal)
                .copied()
                .or_else(|| archived.and_then(|turns| turns.get(ordinal)))
                .cloned()
        })
        .collect()
}

fn materialize_subagent_generation(
    execution: &ActiveExecution,
    agent_id: Uuid,
    generation: u64,
) -> Option<Vec<SubagentConversationTurn>> {
    materialize_generation_from_parts(
        execution.subagent_generations.get(&agent_id).copied()?,
        execution.subagent_conversations.get(&agent_id)?,
        execution.subagent_archived_turns.get(&agent_id),
        execution.subagent_generation_heads.get(&agent_id),
        generation,
    )
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
                "max_duration_seconds": {"type": "integer", "minimum": 1, "maximum": 86400},
                "mode": {
                    "type": "string",
                    "enum": ["inline", "async"],
                    "description": "inline waits for the terminal result; async returns a durable agent_id"
                }
            },
            "required": [
                "role", "input", "max_tokens", "max_cost_cents", "max_duration_seconds"
            ],
            "additionalProperties": false
        }))
        .expect("subagent tool schema is serializable"),
    }
}

fn subagent_wait_tool() -> ModelTool {
    ModelTool {
        name: "agent.wait".into(),
        description: "Wait for one asynchronous subagent without cancelling it on timeout".into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "format": "uuid"},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 300000}
            },
            "required": ["agent_id", "timeout_ms"],
            "additionalProperties": false
        }))
        .expect("subagent wait schema is serializable"),
    }
}

fn subagent_close_tool() -> ModelTool {
    ModelTool {
        name: "agent.close".into(),
        description: "Cancel one asynchronous subagent tree and reap its resources".into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "format": "uuid"}
            },
            "required": ["agent_id"],
            "additionalProperties": false
        }))
        .expect("subagent close schema is serializable"),
    }
}

fn subagent_send_tool() -> ModelTool {
    ModelTool {
        name: "agent.send".into(),
        description: "Start a follow-up turn on a terminal persistent subagent handle".into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "format": "uuid"},
                "generation": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Required after a handle has changed generation"
                },
                "message": {"type": "string", "minLength": 1, "maxLength": 32000},
                "idempotency_key": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "pattern": "^[A-Za-z0-9._:/-]+$",
                    "description": "Caller-stable key; replay with different content is rejected"
                },
                "interrupt": {
                    "type": "boolean",
                    "default": false,
                    "description": "Stop the active child turn before starting this message"
                }
            },
            "required": ["agent_id", "generation", "message", "idempotency_key"],
            "additionalProperties": false
        }))
        .expect("subagent send schema is serializable"),
    }
}

fn subagent_history_tool() -> ModelTool {
    ModelTool {
        name: "agent.history".into(),
        description:
            "Read completed turns from one persistent subagent handle in actual execution order"
                .into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "format": "uuid"},
                "generation": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Read this immutable generation; omit for the current head"
                },
                "after_activation_ordinal": {"type": "integer", "minimum": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            },
            "required": ["agent_id", "limit"],
            "additionalProperties": false
        }))
        .expect("subagent history schema is serializable"),
    }
}

fn subagent_fork_tool() -> ModelTool {
    ModelTool {
        name: "agent.fork".into(),
        description: "Create an independent persistent handle from one completed subagent turn"
            .into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "source_agent_id": {"type": "string", "format": "uuid"},
                "source_generation": {"type": "integer", "minimum": 1},
                "through_activation_ordinal": {"type": "integer", "minimum": 0},
                "max_tokens": {"type": "integer", "minimum": 1},
                "max_cost_cents": {"type": "integer", "minimum": 1},
                "max_duration_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
            },
            "required": [
                "source_agent_id", "source_generation", "through_activation_ordinal", "max_tokens",
                "max_cost_cents", "max_duration_seconds"
            ],
            "additionalProperties": false
        }))
        .expect("subagent fork schema is serializable"),
    }
}

fn subagent_rollback_tool() -> ModelTool {
    ModelTool {
        name: "agent.rollback".into(),
        description:
            "Move one terminal persistent handle to a completed prior turn under a new generation"
                .into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "format": "uuid"},
                "generation": {"type": "integer", "minimum": 1},
                "through_activation_ordinal": {"type": "integer", "minimum": 0}
            },
            "required": ["agent_id", "generation", "through_activation_ordinal"],
            "additionalProperties": false
        }))
        .expect("subagent rollback schema is serializable"),
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

fn deterministic_subagent_turn_id(
    command: &RunExecutionCommand,
    agent_id: Uuid,
    message_sequence: u64,
) -> Uuid {
    let material = format!(
        "agent-runtime-subagent-turn-v1\n{}\n{}\n{}\n{}",
        command.tenant_id, command.run_id, agent_id, message_sequence
    );
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn deterministic_subagent_fork_id(
    command: &RunExecutionCommand,
    tool_call_id: &str,
    source_agent_id: Uuid,
    through_activation_ordinal: u64,
) -> Uuid {
    let material = format!(
        "agent-runtime-subagent-fork-v1\n{}\n{}\n{}\n{}\n{}",
        command.tenant_id,
        command.run_id,
        tool_call_id,
        source_agent_id,
        through_activation_ordinal
    );
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

struct SubagentContinuationBinding<'a> {
    command: &'a RunExecutionCommand,
    agent_id: Uuid,
    generation: u64,
    idempotency_key: &'a str,
    message_sequence: u64,
    child_run_id: Uuid,
    role: &'a str,
    message: &'a str,
    budget: &'a agent_protocol::RunBudget,
    conversation_history: &'a [SubagentConversationTurn],
}

fn subagent_continuation_binding_digest(binding: SubagentContinuationBinding<'_>) -> String {
    let material = if binding.generation == 1 {
        serde_json::json!({
            "schema_version": 2,
            "tenant_id": binding.command.tenant_id,
            "parent_run_id": binding.command.run_id,
            "agent_id": binding.agent_id,
            "idempotency_key": binding.idempotency_key,
            "message_sequence": binding.message_sequence,
            "child_run_id": binding.child_run_id,
            "role": binding.role,
            "message": binding.message,
            "budget": binding.budget,
            "conversation_history_digest":
                agent_protocol::subagent_conversation_history_digest(binding.conversation_history),
        })
    } else {
        serde_json::json!({
            "schema_version": 3,
            "tenant_id": binding.command.tenant_id,
            "parent_run_id": binding.command.run_id,
            "agent_id": binding.agent_id,
            "generation": binding.generation,
            "idempotency_key": binding.idempotency_key,
            "message_sequence": binding.message_sequence,
            "child_run_id": binding.child_run_id,
            "role": binding.role,
            "message": binding.message,
            "budget": binding.budget,
            "conversation_history_digest":
                agent_protocol::subagent_conversation_history_digest(binding.conversation_history),
        })
    };
    digest_bytes(
        &serde_json::to_vec(&material).expect("subagent continuation binding is serializable"),
    )
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
        "mode": arguments.mode,
        "budget": {
            "max_tokens": arguments.max_tokens,
            "max_cost_cents": arguments.max_cost_cents,
            "max_duration_seconds": arguments.max_duration_seconds
        }
    }))
    .expect("subagent binding material is serializable");
    digest_bytes(&material)
}

fn subagent_control_request(
    command: &RunExecutionCommand,
    call: ToolCall,
    effect: ToolEffect,
) -> ToolExecutionRequest {
    let binding_digest = digest_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "tenant_id": command.tenant_id,
            "parent_run_id": command.run_id,
            "parent_attempt_id": command.attempt_id,
            "tool_call": &call
        }))
        .expect("subagent control binding is serializable"),
    );
    ToolExecutionRequest {
        call,
        effect,
        sandbox: SandboxClass::TrustedNative,
        binding_digest,
    }
}

#[derive(Debug)]
struct ApprovalDecisionReceipt {
    command: ToolApprovalDecisionCommand,
    outcome: ToolApprovalOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AppliedApprovalDecision {
    binding_digest: String,
    decision: ToolApprovalDecision,
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

#[derive(Debug)]
pub struct PreparedTranscriptCompaction {
    pub invocation: ModelInvocation,
    pub workload_token: WorkloadToken,
    pub binding_digest: String,
    pub source_message_count: u32,
    pub retained_message_count: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingTranscriptCompaction {
    binding_digest: String,
    source_transcript_digest: String,
    source_prefix_digest: String,
    source_message_count: u32,
    retained_tail_digest: String,
    retained_message_count: u32,
    system_message_count: u32,
    retained_start: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TranscriptCompactionRecord {
    binding_digest: String,
    source_transcript_digest: String,
    source_prefix_digest: String,
    source_message_count: u32,
    retained_tail_digest: String,
    retained_message_count: u32,
    summary_digest: String,
    compacted_transcript_digest: String,
    compacted_message_count: u32,
    summary_message_index: u32,
}

impl PendingTranscriptCompaction {
    fn is_valid_for(&self, command: &RunExecutionCommand, transcript: &[ModelMessage]) -> bool {
        let Ok(system_count) = usize::try_from(self.system_message_count) else {
            return false;
        };
        let Ok(retained_start) = usize::try_from(self.retained_start) else {
            return false;
        };
        if system_count > retained_start || retained_start > transcript.len() {
            return false;
        }
        let source = &transcript[system_count..retained_start];
        let retained = &transcript[retained_start..];
        self.source_message_count == u32::try_from(source.len()).unwrap_or(u32::MAX)
            && self.retained_message_count == u32::try_from(retained.len()).unwrap_or(u32::MAX)
            && !source.is_empty()
            && self.source_transcript_digest == model_messages_digest(transcript)
            && self.source_prefix_digest == model_messages_digest(source)
            && self.retained_tail_digest == model_messages_digest(retained)
            && self.binding_digest
                == compaction_binding_digest(
                    command,
                    &self.source_transcript_digest,
                    &self.source_prefix_digest,
                    self.source_message_count,
                    &self.retained_tail_digest,
                    self.retained_message_count,
                )
    }
}

impl TranscriptCompactionRecord {
    fn is_valid_for(&self, command: &RunExecutionCommand, transcript: &[ModelMessage]) -> bool {
        let Ok(compacted_count) = usize::try_from(self.compacted_message_count) else {
            return false;
        };
        let Ok(summary_index) = usize::try_from(self.summary_message_index) else {
            return false;
        };
        let Ok(retained_count) = usize::try_from(self.retained_message_count) else {
            return false;
        };
        if compacted_count > transcript.len()
            || summary_index >= compacted_count
            || summary_index
                .saturating_add(1)
                .saturating_add(retained_count)
                != compacted_count
        {
            return false;
        }
        let compacted = &transcript[..compacted_count];
        let summary = &transcript[summary_index..summary_index + 1];
        let retained = &transcript[summary_index + 1..compacted_count];
        let summary_is_user_context = transcript[summary_index].role == ModelRole::User as i32
            && transcript[summary_index].content.len() == 1
            && matches!(
                transcript[summary_index].content[0].body.as_ref(),
                Some(content_part::Body::Text(text))
                    if text.text.starts_with(COMPACTION_SUMMARY_PREFIX)
            );
        summary_is_user_context
            && self.summary_digest == model_messages_digest(summary)
            && self.retained_tail_digest == model_messages_digest(retained)
            && self.compacted_transcript_digest == model_messages_digest(compacted)
            && self.binding_digest
                == compaction_binding_digest(
                    command,
                    &self.source_transcript_digest,
                    &self.source_prefix_digest,
                    self.source_message_count,
                    &self.retained_tail_digest,
                    self.retained_message_count,
                )
    }
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
/// - `ApprovalMode::Ask` always. The effect defaults to `Unknown`; only the
///   operator-owned, Run-frozen override map may narrow it. MCP annotations are
///   not an authority for replay or failure semantics.
/// - the frozen catalog digest as the implementation digest, so a Checkpoint
///   restore recomputes it and refuses when the catalog moved.
/// - `tool:mcp:<server>` as the required scope, so a Skill still cannot reach a
///   server the AgentVersion never delegated.
pub fn federated_tool_definitions(
    server_name: &str,
    frozen_catalog_digest: &str,
    tools: impl IntoIterator<Item = (String, String, serde_json::Value)>,
    tool_effect_overrides: &BTreeMap<String, ToolEffect>,
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
        let local_name = qualified_name
            .strip_prefix(&prefix)
            .expect("the namespace prefix was checked above");
        let effect = tool_effect_overrides
            .get(local_name)
            .copied()
            .unwrap_or(ToolEffect::Unknown);
        definitions.push(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: qualified_name,
                effect,
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
pub struct ToolOutcomeUncertainty {
    pub request: ToolExecutionRequest,
    pub source_attempt_id: Uuid,
    pub started_event_id: Uuid,
    pub started_sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunInterruption {
    Cancellation,
    DurationTimeout,
}

impl RunInterruption {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancellation => "cancellation",
            Self::DurationTimeout => "duration_timeout",
        }
    }

    const fn requested_status(self) -> RunStatus {
        match self {
            Self::Cancellation => RunStatus::Cancelled,
            Self::DurationTimeout => RunStatus::TimedOut,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerRecoveryAction {
    InvokeModel,
    PlanPendingTool,
    RetryTool(ToolExecutionRequest),
    RetryToolBatch(Vec<ToolExecutionRequest>),
    WaitForApproval,
    WaitForSubagent,
    WaitForMcpInput(McpInputRequired),
    ResumeMcpTool {
        request: ToolExecutionRequest,
        pending: McpInputRequired,
        continuation: McpInputContinuation,
        dispatch_started: bool,
    },
    TerminateBudgetExceeded(BudgetDimension),
    TerminateIndeterminate(ToolOutcomeUncertainty),
}

fn tool_outcome_uncertainty(
    execution: &ActiveExecution,
) -> Result<Option<ToolOutcomeUncertainty>, WorkerAssignmentError> {
    let uncertain = execution
        .started_tool_calls
        .iter()
        .filter_map(|(tool_call_id, started)| {
            execution
                .outstanding_tool_calls
                .get(tool_call_id)
                .filter(|request| {
                    matches!(
                        request.effect,
                        ToolEffect::NonIdempotent | ToolEffect::Unknown
                    )
                })
                .map(|request| ToolOutcomeUncertainty {
                    request: request.clone(),
                    source_attempt_id: started.attempt_id,
                    started_event_id: started.event_id,
                    started_sequence: started.sequence,
                })
        })
        .collect::<Vec<_>>();
    match uncertain.as_slice() {
        [] => Ok(None),
        [uncertainty] => Ok(Some(uncertainty.clone())),
        _ => Err(WorkerAssignmentError::InvalidCheckpoint(
            "serial worker checkpoint contains multiple ambiguous tools".into(),
        )),
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StagedOrderedToolResult {
    request: ToolExecutionRequest,
    content: serde_json::Value,
    is_error: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct ResolvedMcpInput {
    pending: McpInputRequired,
    continuation: McpInputContinuation,
    resolution_event: EventEnvelope,
    #[serde(default)]
    continuation_started: Option<EventEnvelope>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpInputRequiredReceipt {
    pub event: EventEnvelope,
    pub pending: McpInputRequired,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpInputResolutionReceipt {
    pub event: EventEnvelope,
    pub request: ToolExecutionRequest,
    pub continuation: McpInputContinuation,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkerCheckpointState {
    schema_version: u32,
    runtime_version: String,
    /// Schema 26 binds recovery to the same application inside a tenant.
    #[serde(default)]
    application_id: Uuid,
    #[serde(default)]
    workload_identity_id: Uuid,
    workspace_id: Uuid,
    agent_version_id: Uuid,
    model_policy_id: Uuid,
    input_digest: String,
    /// Schema 16 binds a continuation Run to the exact completed-turn prefix
    /// used to construct its model transcript.
    #[serde(default)]
    subagent_history_digest: String,
    /// Schema 23 binds a root Run to the exact Session branch generation and
    /// immutable completed-turn prefix used to construct its transcript.
    #[serde(default)]
    session_branch: Option<agent_protocol::SessionBranchSnapshot>,
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
    /// Written from schema 6 onwards. Older checkpoints did not bind MCP Tool
    /// names to their frozen per-server catalog digest.
    #[serde(default)]
    federated_tool_bindings: BTreeMap<String, String>,
    /// Written from schema 7 onwards.
    #[serde(default)]
    federated_discovery_policy: Option<CheckpointMcpDiscoveryPolicy>,
    /// Written from schema 9 onwards. Catalog equality does not prove that the
    /// endpoint, server identity, or credential authority stayed the same.
    #[serde(default)]
    federated_server_binding_digest: String,
    /// Written from schema 8 onwards. This is the pre-admission policy from the
    /// command, not a reconstruction from whichever defaults this process has.
    #[serde(default)]
    runtime_policy: Option<RuntimeExecutionPolicySnapshot>,
    transcript: Vec<Vec<u8>>,
    /// Schema 19 binds the exact source and repaired digests for an explicit
    /// lower-authority history import. Authoritative Checkpoints are never
    /// passed through the repair function.
    #[serde(default)]
    history_repair: Option<HistoryRepairReport>,
    /// Schema 17 persists an in-flight, deterministic compaction request before
    /// model egress, plus the latest applied provenance record.
    #[serde(default)]
    pending_transcript_compaction: Option<PendingTranscriptCompaction>,
    #[serde(default)]
    transcript_compaction: Option<TranscriptCompactionRecord>,
    pending_tool_calls: Vec<ToolCall>,
    outstanding_tool_calls: BTreeMap<String, ToolExecutionRequest>,
    started_tool_calls: BTreeMap<String, EventEnvelope>,
    /// Schema 24 retains the assistant's source order independently from
    /// executor completion order.
    #[serde(default)]
    ordered_tool_commit_queue: VecDeque<String>,
    /// Completed later calls wait here until every earlier call can commit.
    #[serde(default)]
    staged_ordered_tool_results: BTreeMap<String, StagedOrderedToolResult>,
    pending_approval: Option<agent_protocol::ToolApprovalRequest>,
    /// Schema 25: a stateless MRTR request parked before asking the user.
    #[serde(default)]
    pending_mcp_input: Option<McpInputRequired>,
    /// Schema 25: user response persisted before continuation dispatch.
    #[serde(default)]
    resolved_mcp_input: Option<ResolvedMcpInput>,
    #[serde(default)]
    pending_subagent: Option<SubagentSpawnRequest>,
    /// Schema 11 stores the whole ordered batch. `pending_subagent` above is a
    /// read-compatible projection of the first entry for older checkpoints.
    #[serde(default)]
    pending_subagents: Vec<SubagentSpawnRequest>,
    /// Schema 13 separates asynchronous live handles from inline spawn calls
    /// that still owe their original Tool result.
    #[serde(default)]
    active_subagents: BTreeMap<Uuid, SubagentSpawnRequest>,
    /// Terminal asynchronous handles stay readable and prevent duplicate
    /// budget settlement after recovery.
    #[serde(default)]
    completed_subagents: BTreeMap<Uuid, SubagentResultDelivery>,
    #[serde(default)]
    subagent_handles: BTreeMap<Uuid, SubagentSpawnRequest>,
    #[serde(default)]
    subagent_message_sequences: BTreeMap<Uuid, u64>,
    /// Schema 14 makes accepted input replayable by caller idempotency key.
    #[serde(default)]
    subagent_message_receipts: BTreeMap<Uuid, BTreeMap<String, SubagentMessageReceipt>>,
    /// Schema 15 stores caller keys in exact FIFO acceptance order per handle.
    #[serde(default)]
    subagent_message_queues: BTreeMap<Uuid, VecDeque<String>>,
    /// Schema 16 stores completed child turns in actual activation order.
    #[serde(default)]
    subagent_conversations: BTreeMap<Uuid, Vec<SubagentConversationTurn>>,
    /// Last activation ordinal assigned per stable handle.
    #[serde(default)]
    subagent_activation_sequences: BTreeMap<Uuid, u64>,
    /// Schema 20 assigns a durable head generation to every stable handle.
    #[serde(default)]
    subagent_generations: BTreeMap<Uuid, u64>,
    /// Schema 20 makes fork creation idempotent by its model Tool call.
    #[serde(default)]
    subagent_fork_receipts: BTreeMap<String, SubagentForkRecord>,
    /// Schema 21 stores superseded turns once, keyed by their globally
    /// monotonic activation ordinal. Active heads never duplicate these turns.
    #[serde(default)]
    subagent_archived_turns: BTreeMap<Uuid, BTreeMap<u64, SubagentConversationTurn>>,
    /// Schema 21 records the immutable ordinal path and digest for every
    /// superseded generation of a stable handle.
    #[serde(default)]
    subagent_generation_heads: BTreeMap<Uuid, BTreeMap<u64, SubagentGenerationHead>>,
    /// Schema 21 makes each generation transition idempotent by Tool call.
    #[serde(default)]
    subagent_rollback_receipts: BTreeMap<String, SubagentRollbackRecord>,
    /// Schema 22 is the authoritative parent-Run reservation ledger. Every
    /// pending, active or queued child execution owns exactly one entry.
    #[serde(default)]
    subagent_budget_reservations: BTreeMap<Uuid, SubagentBudgetReservation>,
    /// Schema 13 records an irreversible lifecycle edge. A closed handle may
    /// still be inspected or closed again, but can never accept new input.
    #[serde(default)]
    closed_subagents: BTreeSet<Uuid>,
    #[serde(default)]
    budget_usage: BudgetUsage,
    /// Schema 12. Active checkpoints charge the wall-clock recovery gap;
    /// approval-parking checkpoints do not.
    #[serde(default)]
    execution_time: Option<CheckpointExecutionTime>,
    #[serde(default)]
    pending_budget_exhaustion: Option<PendingBudgetExhaustion>,
    #[serde(default)]
    steering_receipts: BTreeMap<Uuid, SteeringReceipt>,
    /// Exact decisions already incorporated into this Checkpoint. The full
    /// transport command is not persisted because restore assigns a new
    /// worker/attempt identity; only the reviewed binding remains valid.
    #[serde(default)]
    applied_approval_decisions: BTreeMap<Uuid, AppliedApprovalDecision>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CheckpointExecutionTime {
    elapsed_millis: u64,
    checkpointed_at: DateTime<Utc>,
    active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CheckpointMcpDiscoveryPolicy {
    max_concurrent: u64,
    per_server_timeout_secs: u64,
    per_server_timeout_nanos: u32,
    total_timeout_secs: u64,
    total_timeout_nanos: u32,
    #[serde(default = "single_checkpoint_mcp_attempt")]
    max_attempts_per_server: u8,
    #[serde(default)]
    initial_retry_backoff_millis: u64,
}

const fn single_checkpoint_mcp_attempt() -> u8 {
    1
}

impl From<McpDiscoveryPolicy> for CheckpointMcpDiscoveryPolicy {
    fn from(policy: McpDiscoveryPolicy) -> Self {
        Self {
            max_concurrent: policy.max_concurrent.get() as u64,
            per_server_timeout_secs: policy.per_server_timeout.as_secs(),
            per_server_timeout_nanos: policy.per_server_timeout.subsec_nanos(),
            total_timeout_secs: policy.total_timeout.as_secs(),
            total_timeout_nanos: policy.total_timeout.subsec_nanos(),
            max_attempts_per_server: policy.max_attempts_per_server,
            initial_retry_backoff_millis: u64::try_from(policy.initial_retry_backoff.as_millis())
                .expect("bounded MCP retry backoff fits in u64 milliseconds"),
        }
    }
}

impl CheckpointMcpDiscoveryPolicy {
    fn into_policy(self) -> Option<McpDiscoveryPolicy> {
        let max_concurrent = usize::try_from(self.max_concurrent)
            .ok()
            .and_then(std::num::NonZeroUsize::new)?;
        (self.per_server_timeout_nanos < 1_000_000_000 && self.total_timeout_nanos < 1_000_000_000)
            .then_some(McpDiscoveryPolicy {
                max_concurrent,
                per_server_timeout: Duration::new(
                    self.per_server_timeout_secs,
                    self.per_server_timeout_nanos,
                ),
                total_timeout: Duration::new(self.total_timeout_secs, self.total_timeout_nanos),
                max_attempts_per_server: self.max_attempts_per_server,
                initial_retry_backoff: Duration::from_millis(self.initial_retry_backoff_millis),
            })
    }
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
    #[error("execution workload identity is invalid: {0}")]
    InvalidWorkloadIdentity(String),
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
    #[error("required MCP server discovery failed: {0}")]
    RequiredMcpServersUnavailable(String),
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
    #[error("MCP input request does not match the active Tool execution")]
    McpInputBindingMismatch,
    #[error("MCP input resolution is invalid: {0}")]
    InvalidMcpInputResolution(String),
    #[error("tool result arrived before execution start was durably recorded")]
    ToolExecutionNotStarted,
    #[error("parallel tool batch is invalid or exceeds its frozen execution policy")]
    InvalidParallelToolBatch,
    #[error("subagent result does not match the suspended spawn request")]
    SubagentResultBindingMismatch,
    #[error("subagent message idempotency key was reused with different content")]
    SubagentMessageConflict,
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
    #[error("model transcript is not safe to compact")]
    InvalidTranscriptCompaction,
    #[error("model transcript is invalid: {0}")]
    InvalidTranscript(String),
    #[error("context compaction result does not match the prepared transcript")]
    TranscriptCompactionBindingMismatch,
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
    pub fn verify_execution_workload_identity(
        command: &RunExecutionCommand,
        verifier: &WorkloadTokenVerifier,
        received_at: DateTime<Utc>,
    ) -> Result<(), WorkerAssignmentError> {
        let mut capabilities = vec![
            RequiredCapability::new("model-gateway", "model.execute", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.read", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
        ];
        if !command.mcp_servers.is_empty() {
            capabilities.push(RequiredCapability::new(
                "model-gateway",
                "mcp.federate",
                true,
            ));
        }
        let mut verified_claims = None;
        for capability in capabilities {
            let claims = verifier
                .verify(
                    command.workload_token.as_str(),
                    capability,
                    received_at.timestamp_millis(),
                )
                .map_err(|error| {
                    WorkerAssignmentError::InvalidWorkloadIdentity(error.to_string())
                })?;
            verified_claims.get_or_insert(claims);
        }
        let claims = verified_claims.expect("at least one capability is required");
        let binding = WorkloadIdentityBinding {
            tenant_id: command.tenant_id,
            application_id: command.application_id,
            workload_identity_id: command.workload_identity_id,
            run_id: command.run_id,
            session_id: command.session_id,
            workspace_id: command.workspace_id,
            agent_version_id: command.agent_version_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
        };
        let authorized_mcp_servers = mcp_gateway::authorized_server_digests(&command.mcp_servers)
            .map_err(|error| {
            WorkerAssignmentError::InvalidWorkloadIdentity(error.to_string())
        })?;
        if claims.schema_version != 4
            || !claims.authorizes(&binding)
            || claims.model_policy_id != command.model_policy_id
            || claims.model_policy_digest != command.model_policy_digest
            || claims.authorized_mcp_servers != authorized_mcp_servers
            || claims.issued_at_unix_ms != command.issued_at.timestamp_millis()
            || claims.expires_at_unix_ms != command.lease_expires_at.timestamp_millis()
        {
            return Err(WorkerAssignmentError::WorkloadIdentityBindingMismatch);
        }
        Ok(())
    }

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
        let mut capabilities = vec![
            RequiredCapability::new("model-gateway", "model.execute", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.read", true),
            RequiredCapability::new("checkpoint-gateway", "checkpoint.write", true),
        ];
        if !active.mcp_servers.is_empty() {
            capabilities.push(RequiredCapability::new(
                "model-gateway",
                "mcp.federate",
                true,
            ));
        }
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
        let complete_identity =
            active.schema_version >= RUN_EXECUTION_COMPLETE_IDENTITY_SCHEMA_VERSION;
        let binding = WorkloadIdentityBinding {
            tenant_id: active.tenant_id,
            application_id: if complete_identity {
                active.application_id
            } else {
                Uuid::nil()
            },
            workload_identity_id: if complete_identity {
                active.workload_identity_id
            } else {
                Uuid::nil()
            },
            run_id: active.run_id,
            session_id: if complete_identity {
                active.session_id
            } else {
                Uuid::nil()
            },
            workspace_id: if complete_identity {
                active.workspace_id
            } else {
                Uuid::nil()
            },
            agent_version_id: if complete_identity {
                active.agent_version_id
            } else {
                Uuid::nil()
            },
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
        };
        let mcp_server_binding_matches = if complete_identity {
            claims.schema_version == 4
                && claims.authorized_mcp_servers
                    == mcp_gateway::authorized_server_digests(&active.mcp_servers).map_err(
                        |error| {
                            WorkerAssignmentError::InvalidWorkloadIdentityRenewal(error.to_string())
                        },
                    )?
        } else {
            matches!(claims.schema_version, 2 | 3) && claims.authorized_mcp_servers.is_empty()
        };
        if !claims.authorizes(&binding)
            || claims.model_policy_id != active.model_policy_id
            || claims.model_policy_digest != active.model_policy_digest
            || !mcp_server_binding_matches
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
    /// Attaches the federated Tools discovered for one attempt.
    ///
    /// Separate from `accept` because discovery needs the network and `accept`
    /// is synchronous. Until this runs the Run has no federated Tools at all,
    /// which is the safe reading -- a model cannot call what was never offered.
    pub fn attach_federated_tools(
        &mut self,
        attempt_id: Uuid,
        registry: ToolRegistry,
        definitions: Vec<WorkerToolDefinition>,
        executors: Vec<(String, Arc<dyn ToolExecutor>)>,
        policy: McpDiscoveryPolicy,
    ) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        // Only tools that ended up with an executor. A definition without one
        // would be offered to the model and then fail to launch, which is worse
        // than never offering it.
        let runnable = executors
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        let definitions = definitions
            .into_iter()
            .filter(|definition| runnable.contains(&definition.descriptor.name))
            .collect::<Vec<_>>();
        let definitions_by_name = definitions
            .iter()
            .map(|definition| (definition.descriptor.name.as_str(), definition))
            .collect::<BTreeMap<_, _>>();
        let mut attached_executors = HashMap::new();
        for (name, executor) in executors {
            let Some(definition) = definitions_by_name.get(name.as_str()) else {
                continue;
            };
            if executor.implementation_digest() != definition.descriptor.implementation_digest {
                return Err(WorkerAssignmentError::ToolExecutorConfiguration(format!(
                    "federated tool {name} executor implementation does not match its catalog"
                )));
            }
            attached_executors.insert(name, executor);
        }
        let bindings = definitions
            .iter()
            .map(|definition| {
                (
                    definition.descriptor.name.clone(),
                    definition.descriptor.implementation_digest.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if execution
            .expected_federated_tool_bindings
            .as_ref()
            .is_some_and(|expected| expected != &bindings)
        {
            return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
        }
        if execution
            .expected_federated_discovery_policy
            .is_some_and(|expected| expected != policy)
        {
            return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
        }
        execution.federated_definitions = definitions;
        execution.federated_registry = Some(registry);
        execution.federated_executors = FederatedExecutors(attached_executors);
        execution.federated_tool_bindings = bindings;
        execution.federated_discovery_policy = Some(policy);
        Ok(())
    }

    pub fn bind_runtime_mcp_read_servers(
        &mut self,
        attempt_id: Uuid,
        servers: BTreeMap<String, mcp_gateway::RuntimeMcpReadServerBinding>,
    ) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        for (name, binding) in &servers {
            if name != &binding.server.name
                || !execution
                    .command
                    .mcp_servers
                    .iter()
                    .any(|server| server == &binding.server)
                || !execution
                    .command
                    .delegated_scopes
                    .contains(&format!("mcp:read:{name}"))
                || binding.frozen_catalog_digest.len() != 64
                || (!binding
                    .capabilities
                    .contains(&McpServerCapability::Resources)
                    && !binding.capabilities.contains(&McpServerCapability::Prompts))
            {
                return Err(WorkerAssignmentError::ToolConfiguration(
                    "Runtime MCP read server binding is invalid".into(),
                ));
            }
        }
        let expected_names = execution
            .federated_definitions
            .iter()
            .filter(|definition| mcp_gateway::is_runtime_mcp_read_tool(&definition.descriptor.name))
            .map(|definition| definition.descriptor.name.clone())
            .collect::<BTreeSet<_>>();
        let executable_names = execution
            .federated_executors
            .0
            .keys()
            .filter(|name| mcp_gateway::is_runtime_mcp_read_tool(name))
            .cloned()
            .collect::<BTreeSet<_>>();
        if expected_names != executable_names || (servers.is_empty() != expected_names.is_empty()) {
            return Err(WorkerAssignmentError::ToolExecutorConfiguration(
                "Runtime MCP read Tool definitions and executors do not match".into(),
            ));
        }
        execution.runtime_mcp_read_servers = servers;
        Ok(())
    }

    pub fn federated_executor(
        &self,
        attempt_id: Uuid,
        tool_name: &str,
    ) -> Option<Arc<dyn ToolExecutor>> {
        self.accepted
            .get(&attempt_id)?
            .federated_executors
            .0
            .get(tool_name)
            .cloned()
    }

    /// Confirms that a restored schema-6+ checkpoint has regained every exact
    /// federated Tool binding before model work or an old approval may resume.
    pub fn verify_restored_federated_tools(
        &self,
        attempt_id: Uuid,
    ) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let Some(expected) = execution.expected_federated_tool_bindings.as_ref() else {
            if execution.restored_from_checkpoint.is_some()
                && !execution.command.mcp_servers.is_empty()
            {
                // Before schema 6, MCP catalogs were not persisted at all. A
                // re-discovery can therefore never prove it matches the catalog
                // that produced an old approval or model-visible Tool set.
                return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
            }
            return Ok(());
        };
        let Some(expected_policy) = execution.expected_federated_discovery_policy else {
            if execution.restored_from_checkpoint.is_some()
                && !execution.command.mcp_servers.is_empty()
            {
                return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
            }
            return Ok(());
        };
        if execution.federated_discovery_policy != Some(expected_policy) {
            return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
        }
        let attached = execution
            .federated_executors
            .0
            .keys()
            .map(|name| {
                execution
                    .federated_tool_bindings
                    .get(name)
                    .map(|digest| (name.clone(), digest.clone()))
            })
            .collect::<Option<BTreeMap<_, _>>>();
        if attached.as_ref() != Some(expected) {
            return Err(WorkerAssignmentError::CheckpointToolCatalogMismatch);
        }
        Ok(())
    }

    /// Federated Tool definitions to offer the model for one attempt.
    pub fn federated_tool_definitions_for(&self, attempt_id: Uuid) -> Vec<String> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.federated_executors.0.keys().cloned().collect())
            .unwrap_or_default()
    }

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
    pub fn expired_duration_attempt_ids(&self) -> Vec<Uuid> {
        let mut attempts = self
            .accepted
            .iter()
            .filter(|(_, execution)| {
                execution.terminal_event.is_none()
                    && !execution.machine.status().is_terminal()
                    && execution.execution_time.is_active()
                    && execution
                        .execution_time
                        .remaining(execution.command.budget.max_duration_seconds)
                        .is_zero()
            })
            .map(|(attempt_id, _)| *attempt_id)
            .collect::<Vec<_>>();
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
        let repaired_history = command
            .history_import
            .as_ref()
            .map(repair_imported_history)
            .transpose()
            .map_err(|error| WorkerAssignmentError::InvalidTranscript(error.to_string()))?;

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
        let session_message_count = command.session_branch.as_ref().map_or(0, |branch| {
            branch
                .history
                .iter()
                .map(|turn| turn.transcript.len())
                .sum()
        });
        let mut transcript = Vec::with_capacity(
            2_usize
                .saturating_add(session_message_count)
                .saturating_add(command.subagent_history.len().saturating_mul(2)),
        );
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
        if let Some(repaired) = &repaired_history {
            transcript.extend(repaired.messages.iter().map(model_message_from_protocol));
        }
        if let Some(branch) = &command.session_branch {
            for turn in &branch.history {
                transcript.extend(turn.transcript.iter().map(model_message_from_protocol));
            }
        }
        for turn in &command.subagent_history {
            if turn.result.transcript.is_empty() {
                transcript.push(ModelMessage {
                    role: ModelRole::User as i32,
                    content: vec![ContentPart {
                        body: Some(content_part::Body::Text(TextPart {
                            text: turn.input.clone(),
                        })),
                    }],
                });
                transcript.push(ModelMessage {
                    role: ModelRole::Assistant as i32,
                    content: vec![ContentPart {
                        body: Some(content_part::Body::Text(TextPart {
                            text: subagent_turn_assistant_text(turn),
                        })),
                    }],
                });
            } else {
                transcript.extend(
                    turn.result
                        .transcript
                        .iter()
                        .map(model_message_from_protocol),
                );
            }
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
                history_repair: repaired_history.map(|repaired| repaired.report),
                assistant_text_buffer: String::new(),
                assistant_rich_content_buffer: Vec::new(),
                pending_transcript_compaction: None,
                transcript_compaction: None,
                effective_agent_instructions: effective_skill_state.agent_instructions,
                effective_tool_names: effective_skill_state.tool_names,
                effective_tool_catalog_digest: effective_skill_state.tool_catalog_digest,
                effective_skill_binding_digest: effective_skill_state.skill_binding_digest,
                pending_tool_calls: VecDeque::new(),
                outstanding_tool_calls: HashMap::new(),
                started_tool_calls: HashMap::new(),
                ordered_tool_commit_queue: VecDeque::new(),
                staged_ordered_tool_results: BTreeMap::new(),
                recovery_replanned_tools: HashMap::new(),
                rebound_approval_event: None,
                pending_approval: None,
                pending_mcp_input: None,
                resolved_mcp_input: None,
                pending_subagents: Vec::new(),
                active_subagents: BTreeMap::new(),
                completed_subagents: BTreeMap::new(),
                subagent_handles: BTreeMap::new(),
                subagent_message_sequences: BTreeMap::new(),
                subagent_message_receipts: BTreeMap::new(),
                subagent_message_queues: BTreeMap::new(),
                subagent_conversations: BTreeMap::new(),
                subagent_activation_sequences: BTreeMap::new(),
                subagent_generations: BTreeMap::new(),
                subagent_fork_receipts: BTreeMap::new(),
                subagent_archived_turns: BTreeMap::new(),
                subagent_generation_heads: BTreeMap::new(),
                subagent_rollback_receipts: BTreeMap::new(),
                subagent_budget_reservations: BTreeMap::new(),
                closed_subagents: BTreeSet::new(),
                subagent_result_receipts: HashMap::new(),
                steering_receipts: HashMap::new(),
                budget_usage: BudgetUsage::default(),
                execution_time: ExecutionTimeBudget::new(),
                // Discovery needs the network, and accept() is synchronous.
                // Attached by the transport once the Run is admitted; until then
                // the Run simply has no federated Tools, which is the safe
                // reading rather than a half-configured one.
                federated_registry: None,
                federated_executors: FederatedExecutors::default(),
                federated_definitions: Vec::new(),
                federated_tool_bindings: BTreeMap::new(),
                expected_federated_tool_bindings: None,
                federated_discovery_policy: None,
                expected_federated_discovery_policy: None,
                runtime_mcp_read_servers: BTreeMap::new(),
                pending_budget_exhaustion: None,
                approval_decisions: HashMap::new(),
                applied_approval_decisions: HashMap::new(),
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
        let state = WorkerCheckpointState {
            schema_version: WORKER_CHECKPOINT_SCHEMA_VERSION,
            runtime_version: self.runtime_version.clone(),
            application_id: execution.command.application_id,
            workload_identity_id: execution.command.workload_identity_id,
            workspace_id: execution.command.workspace_id,
            agent_version_id: execution.command.agent_version_id,
            model_policy_id: execution.command.model_policy_id,
            input_digest: digest_bytes(execution.command.input.as_bytes()),
            subagent_history_digest: agent_protocol::subagent_conversation_history_digest(
                &execution.command.subagent_history,
            ),
            session_branch: execution.command.session_branch.clone(),
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
            federated_tool_bindings: execution.federated_tool_bindings.clone(),
            federated_discovery_policy: execution.federated_discovery_policy.map(Into::into),
            federated_server_binding_digest: mcp_server_binding_digest(
                execution.command.schema_version,
                &execution.command.mcp_servers,
            ),
            runtime_policy: execution.command.runtime_policy.clone(),
            transcript: execution
                .transcript
                .iter()
                .map(Message::encode_to_vec)
                .collect(),
            history_repair: execution.history_repair.clone(),
            pending_transcript_compaction: execution.pending_transcript_compaction.clone(),
            transcript_compaction: execution.transcript_compaction.clone(),
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
            ordered_tool_commit_queue: execution.ordered_tool_commit_queue.clone(),
            staged_ordered_tool_results: execution.staged_ordered_tool_results.clone(),
            pending_approval: execution.pending_approval.clone(),
            pending_mcp_input: execution.pending_mcp_input.clone(),
            resolved_mcp_input: execution.resolved_mcp_input.clone(),
            pending_subagent: execution.pending_subagents.first().cloned(),
            pending_subagents: execution.pending_subagents.clone(),
            active_subagents: execution.active_subagents.clone(),
            completed_subagents: execution.completed_subagents.clone(),
            subagent_handles: execution.subagent_handles.clone(),
            subagent_message_sequences: execution.subagent_message_sequences.clone(),
            subagent_message_receipts: execution.subagent_message_receipts.clone(),
            subagent_message_queues: execution.subagent_message_queues.clone(),
            subagent_conversations: execution.subagent_conversations.clone(),
            subagent_activation_sequences: execution.subagent_activation_sequences.clone(),
            subagent_generations: execution.subagent_generations.clone(),
            subagent_fork_receipts: execution.subagent_fork_receipts.clone(),
            subagent_archived_turns: execution.subagent_archived_turns.clone(),
            subagent_generation_heads: execution.subagent_generation_heads.clone(),
            subagent_rollback_receipts: execution.subagent_rollback_receipts.clone(),
            subagent_budget_reservations: execution.subagent_budget_reservations.clone(),
            closed_subagents: execution.closed_subagents.clone(),
            budget_usage: execution.budget_usage,
            execution_time: Some(execution.execution_time.checkpoint(Utc::now())),
            pending_budget_exhaustion: execution.pending_budget_exhaustion,
            steering_receipts: execution
                .steering_receipts
                .iter()
                .map(|(id, receipt)| (*id, receipt.clone()))
                .collect(),
            applied_approval_decisions: execution
                .applied_approval_decisions
                .iter()
                .map(|(id, decision)| (*id, decision.clone()))
                .collect(),
        };
        let state = serde_json::to_vec(&state)
            .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        Ok(execution.machine.checkpoint(state))
    }

    /// Returns whether the accepted attempt is a delegated child Run.
    ///
    /// The standalone Host uses this narrow identity fact to durably capture a
    /// child transcript before publishing its terminal event. Root Runs retain
    /// their last resumable Checkpoint rather than being overwritten by a
    /// terminal snapshot.
    pub fn is_subagent_attempt(&self, attempt_id: Uuid) -> Result<bool, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        Ok(execution.command.lineage.parent_run_id.is_some())
    }

    /// Returns whether this root attempt is bound to an authoritative Session
    /// branch. The standalone Host uses this to persist the terminal transcript
    /// before publishing the terminal event, closing the crash window between
    /// a completed model turn and advancement of the Session head.
    pub fn has_session_branch(&self, attempt_id: Uuid) -> Result<bool, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        Ok(execution.command.session_branch.is_some())
    }

    /// Audit record for an explicit lower-authority history import, if this
    /// attempt was created through that boundary.
    pub fn history_repair_report(
        &self,
        attempt_id: Uuid,
    ) -> Result<Option<HistoryRepairReport>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.history_repair.clone())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn conversation_transcript(
        &self,
        attempt_id: Uuid,
    ) -> Result<Vec<agent_protocol::Message>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        execution
            .transcript
            .iter()
            .map(protocol_message_from_model)
            .filter_map(Result::transpose)
            .collect()
    }

    pub fn conversation_transcript_from_checkpoint(
        checkpoint: &agent_protocol::CheckpointSnapshot,
    ) -> Result<Vec<agent_protocol::Message>, WorkerAssignmentError> {
        if !checkpoint.verify_digest() || !checkpoint.status.is_terminal() {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "terminal transcript checkpoint identity is invalid".into(),
            ));
        }
        let state: WorkerCheckpointState = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        if !(1..=WORKER_CHECKPOINT_SCHEMA_VERSION).contains(&state.schema_version) {
            return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                "unsupported schema version {}",
                state.schema_version
            )));
        }
        state
            .transcript
            .iter()
            .map(|encoded| {
                ModelMessage::decode(encoded.as_slice())
                    .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))
            })
            .map(|message| message.and_then(|message| protocol_message_from_model(&message)))
            .filter_map(Result::transpose)
            .collect()
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
            application_id: command.application_id,
            workload_identity_id: command.workload_identity_id,
            run_id: command.run_id,
            session_id: command.session_id,
            workspace_id: command.workspace_id,
            agent_version_id: command.agent_version_id,
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
        let expected_history_repair = command
            .history_import
            .as_ref()
            .map(repair_imported_history)
            .transpose()
            .map_err(|error| WorkerAssignmentError::InvalidTranscript(error.to_string()))?
            .map(|repaired| repaired.report);
        if checkpoint.status.is_terminal() || checkpoint.attempt_id == command.attempt_id {
            return Err(WorkerAssignmentError::CheckpointIdentityMismatch);
        }
        let mut state: WorkerCheckpointState = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))?;
        if !(1..=WORKER_CHECKPOINT_SCHEMA_VERSION).contains(&state.schema_version) {
            return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                "unsupported schema version {}",
                state.schema_version
            )));
        }
        if state.schema_version < 15 {
            // Schema 14 receipts predate both the mailbox and interrupt flag.
            // They represented exactly one immediate successor: active when
            // its child binding is still running, completed otherwise.
            for (agent_id, receipts) in &mut state.subagent_message_receipts {
                for receipt in receipts.values_mut() {
                    receipt.interrupt = false;
                    receipt.status = if state.active_subagents.get(agent_id).is_some_and(|active| {
                        active.binding_digest == receipt.child_request.binding_digest
                    }) {
                        SubagentMessageStatus::Active
                    } else {
                        SubagentMessageStatus::Completed
                    };
                }
            }
            state.subagent_message_queues.clear();
        }
        if state.schema_version < 16 {
            // Legacy checkpoints retained only the latest terminal result, so
            // a complete multi-turn transcript cannot be invented during
            // migration. Preserve the verifiable latest turn when available;
            // an active legacy successor resumes with its original context.
            state.subagent_conversations.clear();
            state.subagent_activation_sequences.clear();
            for request in state
                .pending_subagents
                .iter_mut()
                .chain(state.pending_subagent.iter_mut())
                .chain(state.active_subagents.values_mut())
                .chain(state.subagent_handles.values_mut())
            {
                request.conversation_history.clear();
            }
            for receipts in state.subagent_message_receipts.values_mut() {
                for receipt in receipts.values_mut() {
                    receipt.child_request.conversation_history.clear();
                }
            }
            for agent_id in state.subagent_handles.keys() {
                state.subagent_conversations.insert(*agent_id, Vec::new());
                state.subagent_activation_sequences.insert(*agent_id, 0);
            }
            for (agent_id, result) in &state.completed_subagents {
                let Some(request) = state.subagent_handles.get(agent_id) else {
                    continue;
                };
                if request.delegation_id != result.child_run_id
                    || request.binding_digest != result.binding_digest
                {
                    continue;
                }
                let message_sequence = state
                    .subagent_message_receipts
                    .get(agent_id)
                    .and_then(|receipts| {
                        receipts.values().find(|receipt| {
                            receipt.child_request.binding_digest == request.binding_digest
                        })
                    })
                    .map_or(0, |receipt| receipt.message_sequence);
                let migrated = vec![SubagentConversationTurn {
                    activation_ordinal: 0,
                    message_sequence,
                    child_run_id: request.delegation_id,
                    input: request.input.clone(),
                    result: result.clone(),
                }];
                if !agent_protocol::subagent_conversation_history_is_well_formed(&migrated) {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "legacy subagent result {agent_id} is malformed"
                    )));
                }
                state.subagent_conversations.insert(*agent_id, migrated);
                state.subagent_activation_sequences.insert(*agent_id, 0);
            }
        }
        if state.schema_version < 17
            && (state.pending_transcript_compaction.is_some()
                || state.transcript_compaction.is_some())
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v17 transcript compaction state".into(),
            ));
        }
        let carries_typed_subagent_transcript = state
            .completed_subagents
            .values()
            .any(|result| !result.transcript.is_empty())
            || state
                .subagent_conversations
                .values()
                .flatten()
                .any(|turn| !turn.result.transcript.is_empty())
            || state
                .pending_subagents
                .iter()
                .chain(state.pending_subagent.iter())
                .chain(state.active_subagents.values())
                .chain(state.subagent_handles.values())
                .flat_map(|request| request.conversation_history.iter())
                .any(|turn| !turn.result.transcript.is_empty())
            || state
                .subagent_message_receipts
                .values()
                .flat_map(|receipts| receipts.values())
                .flat_map(|receipt| receipt.child_request.conversation_history.iter())
                .any(|turn| !turn.result.transcript.is_empty());
        if state.schema_version < 18 && carries_typed_subagent_transcript {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v18 typed subagent transcript state".into(),
            ));
        }
        if state.schema_version < 19 && state.history_repair.is_some() {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v19 explicit history repair state".into(),
            ));
        }
        if state.schema_version < 20 {
            if !state.subagent_generations.is_empty() || !state.subagent_fork_receipts.is_empty() {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "legacy checkpoint carries v20 subagent branch state".into(),
                ));
            }
            state.subagent_generations = state
                .subagent_handles
                .keys()
                .map(|agent_id| (*agent_id, 1))
                .collect();
        }
        if state.schema_version < 21
            && (!state.subagent_archived_turns.is_empty()
                || !state.subagent_generation_heads.is_empty()
                || !state.subagent_rollback_receipts.is_empty())
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v21 subagent rollback state".into(),
            ));
        }
        let effective_pending_subagents = if state.pending_subagents.is_empty() {
            state.pending_subagent.iter().cloned().collect::<Vec<_>>()
        } else {
            state.pending_subagents.clone()
        };
        let expected_budget_reservations = rebuild_subagent_budget_reservations(
            &effective_pending_subagents,
            &state.active_subagents,
            &state.subagent_message_receipts,
        )?;
        if state.schema_version < 22 {
            if !state.subagent_budget_reservations.is_empty() {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "legacy checkpoint carries v22 subagent budget reservations".into(),
                ));
            }
            state.subagent_budget_reservations = expected_budget_reservations;
        } else if state.subagent_budget_reservations != expected_budget_reservations {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "subagent budget reservation ledger does not match pending work".into(),
            ));
        }
        if state.schema_version < 23 {
            if state.session_branch.is_some() {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "legacy checkpoint carries v23 root Session branch state".into(),
                ));
            }
        } else if state
            .session_branch
            .as_ref()
            .is_some_and(|branch| !branch.is_well_formed())
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "root Session branch checkpoint state is malformed".into(),
            ));
        }
        if state.schema_version < 24
            && (!state.ordered_tool_commit_queue.is_empty()
                || !state.staged_ordered_tool_results.is_empty())
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v24 ordered Tool batch state".into(),
            ));
        }
        if state.schema_version < 25
            && (state.pending_mcp_input.is_some() || state.resolved_mcp_input.is_some())
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "legacy checkpoint carries v25 MCP input state".into(),
            ));
        }
        let reserved = subagent_budget_reservation_totals(&state.subagent_budget_reservations);
        let used_cost_cents = state.budget_usage.cost_micros.saturating_add(9_999) / 10_000;
        if (state.pending_budget_exhaustion.is_none()
            && (state
                .budget_usage
                .tokens
                .saturating_add(reserved.max_tokens)
                > state.budget.max_tokens
                || used_cost_cents.saturating_add(reserved.max_cost_cents)
                    > state.budget.max_cost_cents))
            || reserved.max_duration_seconds > state.budget.max_duration_seconds
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "subagent budget reservations exceed the parent Run budget".into(),
            ));
        }
        for (agent_id, queue) in &state.subagent_message_queues {
            let mut seen = HashSet::new();
            for key in queue {
                if !seen.insert(key) {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent mailbox {agent_id} contains duplicate key {key}"
                    )));
                }
                let receipt = state
                    .subagent_message_receipts
                    .get(agent_id)
                    .and_then(|receipts| receipts.get(key))
                    .ok_or_else(|| {
                        WorkerAssignmentError::InvalidCheckpoint(format!(
                            "subagent mailbox {agent_id} references missing receipt {key}"
                        ))
                    })?;
                if receipt.status != SubagentMessageStatus::Queued {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent mailbox {agent_id} references non-queued receipt {key}"
                    )));
                }
            }
        }
        for (agent_id, receipts) in &state.subagent_message_receipts {
            for (key, receipt) in receipts {
                if receipt.agent_id != *agent_id || receipt.idempotency_key != *key {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent message receipt identity does not match {agent_id}/{key}"
                    )));
                }
                let queued = state
                    .subagent_message_queues
                    .get(agent_id)
                    .is_some_and(|queue| queue.iter().any(|queued| queued == key));
                match receipt.status {
                    SubagentMessageStatus::Queued if !queued => {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "queued subagent receipt {agent_id}/{key} is absent from its mailbox"
                        )));
                    }
                    SubagentMessageStatus::Active
                        if queued
                            || !state.active_subagents.get(agent_id).is_some_and(|active| {
                                active.binding_digest == receipt.child_request.binding_digest
                            }) =>
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "active subagent receipt {agent_id}/{key} has no matching child"
                        )));
                    }
                    SubagentMessageStatus::Completed | SubagentMessageStatus::Cancelled
                        if queued =>
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "settled subagent receipt {agent_id}/{key} remains queued"
                        )));
                    }
                    _ => {}
                }
            }
        }
        if state.schema_version >= 16 {
            if state.subagent_conversations.len() != state.subagent_handles.len()
                || state.subagent_activation_sequences.len() != state.subagent_handles.len()
                || state.subagent_generations.len() != state.subagent_handles.len()
                || state
                    .subagent_generations
                    .values()
                    .any(|generation| *generation == 0)
                || state
                    .active_subagents
                    .keys()
                    .chain(state.completed_subagents.keys())
                    .chain(state.subagent_message_receipts.keys())
                    .chain(state.subagent_message_queues.keys())
                    .chain(state.subagent_message_sequences.keys())
                    .chain(state.subagent_archived_turns.keys())
                    .chain(state.subagent_generation_heads.keys())
                    .any(|agent_id| !state.subagent_handles.contains_key(agent_id))
                || state
                    .active_subagents
                    .keys()
                    .any(|agent_id| state.completed_subagents.contains_key(agent_id))
            {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "subagent conversation indexes do not match durable handles".into(),
                ));
            }
            for (agent_id, handle) in &state.subagent_handles {
                let history = state.subagent_conversations.get(agent_id).ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent handle {agent_id} has no conversation"
                    ))
                })?;
                if !agent_protocol::subagent_conversation_history_is_well_formed(history) {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent conversation {agent_id} is malformed"
                    )));
                }
                let current_generation = state
                    .subagent_generations
                    .get(agent_id)
                    .copied()
                    .ok_or_else(|| {
                        WorkerAssignmentError::InvalidCheckpoint(format!(
                            "subagent conversation {agent_id} has no generation"
                        ))
                    })?;
                let archived = state.subagent_archived_turns.get(agent_id);
                let heads = state.subagent_generation_heads.get(agent_id);
                if state.schema_version >= 21 {
                    let empty_archived = BTreeMap::new();
                    let archived = archived.unwrap_or(&empty_archived);
                    let empty_heads = BTreeMap::new();
                    let heads = heads.unwrap_or(&empty_heads);
                    if current_generation > SUBAGENT_MAX_GENERATIONS
                        || heads.len()
                            != usize::try_from(current_generation.saturating_sub(1))
                                .unwrap_or(usize::MAX)
                        || (1..current_generation)
                            .any(|generation| !heads.contains_key(&generation))
                        || archived.len() > SUBAGENT_ARCHIVE_MAX_TURNS
                        || serde_json::to_vec(archived)
                            .map_or(true, |encoded| encoded.len() > SUBAGENT_ARCHIVE_MAX_BYTES)
                        || archived.iter().any(|(ordinal, turn)| {
                            *ordinal != turn.activation_ordinal
                                || history
                                    .iter()
                                    .any(|current| current.activation_ordinal == *ordinal)
                        })
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "subagent generation archive {agent_id} is malformed"
                        )));
                    }
                    let mut referenced_archived = BTreeSet::new();
                    for generation in 1..current_generation {
                        let historical = materialize_generation_from_parts(
                            current_generation,
                            history,
                            Some(archived),
                            Some(heads),
                            generation,
                        )
                        .ok_or_else(|| {
                            WorkerAssignmentError::InvalidCheckpoint(format!(
                                "subagent generation {agent_id}/{generation} cannot be materialized"
                            ))
                        })?;
                        let head = heads.get(&generation).expect("generation key was checked");
                        if !agent_protocol::subagent_conversation_history_is_well_formed(
                            &historical,
                        ) || agent_protocol::subagent_conversation_history_digest(&historical)
                            != head.history_digest
                        {
                            return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                                "subagent generation {agent_id}/{generation} digest is invalid"
                            )));
                        }
                        for ordinal in &head.activation_ordinals {
                            if archived.contains_key(ordinal) {
                                referenced_archived.insert(*ordinal);
                            }
                        }
                    }
                    if referenced_archived.len() != archived.len()
                        || archived
                            .keys()
                            .any(|ordinal| !referenced_archived.contains(ordinal))
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "subagent archive {agent_id} contains an unreferenced turn"
                        )));
                    }
                }
                let activation_sequence = state
                    .subagent_activation_sequences
                    .get(agent_id)
                    .copied()
                    .ok_or_else(|| {
                        WorkerAssignmentError::InvalidCheckpoint(format!(
                            "subagent conversation {agent_id} has no activation sequence"
                        ))
                    })?;
                let last_ordinal = history
                    .iter()
                    .map(|turn| turn.activation_ordinal)
                    .chain(archived.into_iter().flat_map(|turns| turns.keys().copied()))
                    .max();
                let expected_sequence = if state.active_subagents.contains_key(agent_id) {
                    last_ordinal.map_or(0, |ordinal| ordinal.saturating_add(1))
                } else {
                    last_ordinal.unwrap_or(0)
                };
                if activation_sequence != expected_sequence {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "subagent conversation {agent_id} activation sequence is inconsistent"
                    )));
                }
                if let Some(active) = state.active_subagents.get(agent_id)
                    && active.conversation_history != *history
                {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                        "active subagent {agent_id} is bound to a different conversation prefix"
                    )));
                }
                if let Some(completed) = state.completed_subagents.get(agent_id) {
                    let Some(last) = history.last() else {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "completed subagent {agent_id} has no terminal conversation turn"
                        )));
                    };
                    if &last.result != completed
                        || last.child_run_id != handle.delegation_id
                        || last.input != handle.input
                        || handle.conversation_history != history[..history.len() - 1]
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                            "completed subagent {agent_id} does not match its conversation tail"
                        )));
                    }
                }
                if let Some(receipts) = state.subagent_message_receipts.get(agent_id) {
                    for receipt in receipts.values() {
                        let prefix_is_preserved = history
                            .starts_with(&receipt.child_request.conversation_history)
                            || (1..current_generation).any(|generation| {
                                materialize_generation_from_parts(
                                    current_generation,
                                    history,
                                    archived,
                                    heads,
                                    generation,
                                )
                                .is_some_and(|historical| {
                                    historical
                                        .starts_with(&receipt.child_request.conversation_history)
                                })
                            });
                        if !receipt.child_request.is_well_formed() || !prefix_is_preserved {
                            return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                                "subagent receipt {agent_id}/{} has an invalid conversation prefix",
                                receipt.idempotency_key
                            )));
                        }
                    }
                }
            }
            for record in state.subagent_fork_receipts.values() {
                let receipt = &record.receipt;
                let Some(source_current) =
                    state.subagent_conversations.get(&receipt.source_agent_id)
                else {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "subagent fork source is missing".into(),
                    ));
                };
                let Some(fork_current) = state.subagent_conversations.get(&receipt.agent_id) else {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "subagent fork target is missing".into(),
                    ));
                };
                let source_current_generation =
                    state.subagent_generations[&receipt.source_agent_id];
                let fork_current_generation = state.subagent_generations[&receipt.agent_id];
                let source_history = materialize_generation_from_parts(
                    source_current_generation,
                    source_current,
                    state.subagent_archived_turns.get(&receipt.source_agent_id),
                    state
                        .subagent_generation_heads
                        .get(&receipt.source_agent_id),
                    receipt.source_generation,
                )
                .ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(
                        "subagent fork source generation is missing".into(),
                    )
                })?;
                let fork_history = materialize_generation_from_parts(
                    fork_current_generation,
                    fork_current,
                    state.subagent_archived_turns.get(&receipt.agent_id),
                    state.subagent_generation_heads.get(&receipt.agent_id),
                    receipt.generation,
                )
                .ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(
                        "subagent fork target generation is missing".into(),
                    )
                })?;
                let prefix_len = source_history
                    .iter()
                    .position(|turn| turn.activation_ordinal == receipt.through_activation_ordinal)
                    .map(|index| index + 1)
                    .ok_or_else(|| {
                        WorkerAssignmentError::InvalidCheckpoint(
                            "subagent fork boundary is missing".into(),
                        )
                    })?;
                let source_prefix = &source_history[..prefix_len];
                if !receipt.is_well_formed()
                    || state
                        .subagent_generations
                        .get(&receipt.source_agent_id)
                        .is_none_or(|generation| *generation < receipt.source_generation)
                    || state
                        .subagent_generations
                        .get(&receipt.agent_id)
                        .is_none_or(|generation| *generation < receipt.generation)
                    || agent_protocol::subagent_conversation_history_digest(source_prefix)
                        != receipt.source_history_digest
                    || !fork_history.starts_with(source_prefix)
                {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "subagent fork provenance is inconsistent".into(),
                    ));
                }
            }
            for (tool_call_id, record) in &state.subagent_rollback_receipts {
                let receipt = &record.receipt;
                let Some(current_history) = state.subagent_conversations.get(&receipt.agent_id)
                else {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "subagent rollback target is missing".into(),
                    ));
                };
                let current_generation = state.subagent_generations[&receipt.agent_id];
                let previous_history = materialize_generation_from_parts(
                    current_generation,
                    current_history,
                    state.subagent_archived_turns.get(&receipt.agent_id),
                    state.subagent_generation_heads.get(&receipt.agent_id),
                    receipt.from_generation,
                );
                let restored_generation = materialize_generation_from_parts(
                    current_generation,
                    current_history,
                    state.subagent_archived_turns.get(&receipt.agent_id),
                    state.subagent_generation_heads.get(&receipt.agent_id),
                    receipt.generation,
                );
                let restored_prefix = restored_generation.as_ref().and_then(|history| {
                    history
                        .iter()
                        .position(|turn| {
                            turn.activation_ordinal == receipt.through_activation_ordinal
                        })
                        .map(|index| &history[..=index])
                });
                if tool_call_id != &receipt.tool_call_id
                    || !receipt.is_well_formed()
                    || current_generation < receipt.generation
                    || previous_history.as_ref().is_none_or(|history| {
                        agent_protocol::subagent_conversation_history_digest(history)
                            != receipt.previous_history_digest
                    })
                    || restored_prefix.is_none_or(|history| {
                        agent_protocol::subagent_conversation_history_digest(history)
                            != receipt.restored_history_digest
                    })
                {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "subagent rollback provenance is inconsistent".into(),
                    ));
                }
            }
        }
        if checkpoint.tenant_id != command.tenant_id
            || checkpoint.run_id != command.run_id
            || checkpoint.session_id != command.session_id
            || (state.schema_version >= 26 && state.application_id != command.application_id)
            || (state.schema_version >= 26
                && state.workload_identity_id != command.workload_identity_id)
            || (command.schema_version >= 20 && state.schema_version < 26)
            || state.workspace_id != command.workspace_id
            || state.agent_version_id != command.agent_version_id
            || state.model_policy_id != command.model_policy_id
            || state.input_digest != digest_bytes(command.input.as_bytes())
            || (state.schema_version >= 16
                && state.subagent_history_digest
                    != agent_protocol::subagent_conversation_history_digest(
                        &command.subagent_history,
                    ))
            || (state.schema_version < 16 && !command.subagent_history.is_empty())
            || (state.schema_version >= 23 && state.session_branch != command.session_branch)
            || (command.schema_version >= 16 && state.schema_version < 23)
            || (command.schema_version >= 17 && state.schema_version < 24)
            || state.agent_instructions_digest
                != digest_bytes(effective_skill_state.agent_instructions.as_bytes())
            || (state.schema_version >= 5
                && state.skill_binding_digest != effective_skill_state.skill_binding_digest)
            || state.lineage != command.lineage
            || (state.schema_version >= 3 && state.subagent_roles != command.subagent_roles)
            || state.budget != command.budget
            || state.delegated_scopes != command.delegated_scopes
            || (state.schema_version >= 8 && state.runtime_policy != command.runtime_policy)
            || (state.schema_version >= 19 && state.history_repair != expected_history_repair)
            || (state.schema_version >= 9
                && state.federated_server_binding_digest
                    != mcp_server_binding_digest(command.schema_version, &command.mcp_servers))
            || (command.schema_version >= 10 && state.schema_version < 8)
            || (command.schema_version >= 11 && state.schema_version < 10)
            || (command.schema_version >= 13 && state.schema_version < 17)
            || (command.schema_version >= 15 && state.schema_version < 19)
            || (!command.mcp_servers.is_empty() && state.schema_version < 9)
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
            if !state.outstanding_tool_calls.contains_key(tool_call_id) {
                return Err(WorkerAssignmentError::InvalidCheckpoint(format!(
                    "started tool {tool_call_id} has no execution request"
                )));
            }
        }
        if state.pending_mcp_input.is_some() && state.resolved_mcp_input.is_some() {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "MCP input cannot be both pending and resolved".into(),
            ));
        }
        let mcp_input_state = state
            .pending_mcp_input
            .as_ref()
            .map(|pending| (pending, None))
            .or_else(|| {
                state
                    .resolved_mcp_input
                    .as_ref()
                    .map(|resolved| (&resolved.pending, Some(resolved)))
            });
        if let Some((pending, resolved)) = mcp_input_state {
            pending.validate().map_err(|error| {
                WorkerAssignmentError::InvalidCheckpoint(format!(
                    "MCP input request is invalid: {error}"
                ))
            })?;
            let request = state
                .outstanding_tool_calls
                .get(&pending.tool_call_id)
                .filter(|request| request.binding_digest == pending.binding_digest)
                .ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(
                        "MCP input has no matching outstanding Tool".into(),
                    )
                })?;
            if !state.started_tool_calls.contains_key(&pending.tool_call_id) {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "MCP input belongs to a Tool that was never durably started".into(),
                ));
            }
            let server = command
                .mcp_servers
                .iter()
                .find(|server| {
                    server.server_id == pending.server_id && server.name == pending.server_name
                })
                .filter(|server| {
                    server.protocol_revision == McpProtocolRevision::V2026_07_28
                        && server
                            .client_capabilities
                            .contains(&McpClientCapability::Elicitation)
                })
                .ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(
                        "MCP input server is absent or lacks frozen elicitation authority".into(),
                    )
                })?;
            if !request
                .call
                .name
                .starts_with(&format!("mcp:{}/", server.name))
            {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "MCP input server does not own the outstanding Tool".into(),
                ));
            }
            match resolved {
                None if checkpoint.status == RunStatus::Suspended => {}
                Some(resolved) if checkpoint.status == RunStatus::Running => {
                    if resolved.continuation.round != pending.round.saturating_add(1)
                        || !(2..=10).contains(&resolved.continuation.round)
                        || resolved.continuation.request_state != pending.request_state
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(
                            "MCP continuation does not preserve its exact round and opaque state"
                                .into(),
                        ));
                    }
                    let issued_at = command.issued_at;
                    McpInputResolutionCommand {
                        schema_version: agent_protocol::MCP_INPUT_RESOLUTION_SCHEMA_VERSION,
                        message_id: Uuid::now_v7(),
                        tenant_id: command.tenant_id,
                        run_id: command.run_id,
                        attempt_id: command.attempt_id,
                        worker_id: command.worker_id,
                        worker_incarnation_id: command.worker_incarnation_id,
                        input_id: pending.input_id,
                        input_version: 1,
                        binding_digest: pending.binding_digest.clone(),
                        responses: resolved.continuation.responses.clone(),
                        issued_at,
                        expires_at: issued_at + chrono::Duration::minutes(1),
                    }
                    .validate_for(pending)
                    .map_err(|error| {
                        WorkerAssignmentError::InvalidCheckpoint(format!(
                            "MCP continuation responses are invalid: {error}"
                        ))
                    })?;
                    if resolved.resolution_event.event_type != "mcp.input.resolved"
                        || resolved.resolution_event.tenant_id != command.tenant_id
                        || resolved.resolution_event.run_id != command.run_id
                        || resolved.resolution_event.session_id != command.session_id
                        || resolved.continuation_started.as_ref().is_some_and(|event| {
                            event.event_type != "mcp.input.continuation.started"
                                || event.tenant_id != command.tenant_id
                                || event.run_id != command.run_id
                                || event.session_id != command.session_id
                        })
                    {
                        return Err(WorkerAssignmentError::InvalidCheckpoint(
                            "MCP continuation event receipts are invalid".into(),
                        ));
                    }
                }
                _ => {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "MCP input state does not match the Run status".into(),
                    ));
                }
            }
        }
        if !state.ordered_tool_commit_queue.is_empty() {
            let max_concurrent_tools = command
                .runtime_policy
                .as_ref()
                .map_or(1, |policy| policy.tool_execution.max_concurrent_tools);
            let ordered = state
                .ordered_tool_commit_queue
                .iter()
                .collect::<BTreeSet<_>>();
            if state.schema_version < 24
                || state.ordered_tool_commit_queue.len() < 2
                || state.ordered_tool_commit_queue.len() > usize::from(max_concurrent_tools)
                || ordered.len() != state.ordered_tool_commit_queue.len()
                || state.pending_approval.is_some()
                || state.outstanding_tool_calls.len() + state.staged_ordered_tool_results.len()
                    != state.ordered_tool_commit_queue.len()
            {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "ordered Tool batch shape is invalid".into(),
                ));
            }
            for tool_call_id in &state.ordered_tool_commit_queue {
                let outstanding = state.outstanding_tool_calls.get(tool_call_id);
                let staged = state.staged_ordered_tool_results.get(tool_call_id);
                if outstanding.is_some() == staged.is_some()
                    || outstanding.is_some_and(|request| request.effect != ToolEffect::Pure)
                    || staged.is_some_and(|result| {
                        result.request.call.id != *tool_call_id
                            || result.request.effect != ToolEffect::Pure
                    })
                {
                    return Err(WorkerAssignmentError::InvalidCheckpoint(
                        "ordered Tool batch binding is invalid".into(),
                    ));
                }
            }
            if state
                .outstanding_tool_calls
                .keys()
                .chain(state.staged_ordered_tool_results.keys())
                .any(|tool_call_id| !ordered.contains(tool_call_id))
            {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "ordered Tool batch contains unbound work".into(),
                ));
            }
        } else if !state.staged_ordered_tool_results.is_empty() {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "staged Tool results have no commit order".into(),
            ));
        }
        let transcript = state
            .transcript
            .iter()
            .map(|encoded| {
                ModelMessage::decode(encoded.as_slice())
                    .map_err(|error| WorkerAssignmentError::InvalidCheckpoint(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if state.schema_version >= 17
            && (state
                .pending_transcript_compaction
                .as_ref()
                .is_some_and(|pending| !pending.is_valid_for(&command, &transcript))
                || state
                    .transcript_compaction
                    .as_ref()
                    .is_some_and(|record| !record.is_valid_for(&command, &transcript)))
        {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "transcript compaction provenance is invalid".into(),
            ));
        }
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
        let expected_federated_tool_bindings =
            (state.schema_version >= 6).then(|| state.federated_tool_bindings.clone());
        let restored_federated_discovery_policy = state
            .federated_discovery_policy
            .and_then(CheckpointMcpDiscoveryPolicy::into_policy);
        let expected_federated_discovery_policy = (state.schema_version >= 7)
            .then_some(restored_federated_discovery_policy)
            .flatten();
        let ambiguous_started_tool_calls = state
            .started_tool_calls
            .iter()
            .filter(|(tool_call_id, _)| {
                state
                    .outstanding_tool_calls
                    .get(*tool_call_id)
                    .is_some_and(|request| {
                        matches!(
                            request.effect,
                            ToolEffect::NonIdempotent | ToolEffect::Unknown
                        )
                    })
            })
            .map(|(tool_call_id, event)| (tool_call_id.clone(), event.clone()))
            .collect();
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
                history_repair: state.history_repair,
                assistant_text_buffer: String::new(),
                assistant_rich_content_buffer: Vec::new(),
                pending_transcript_compaction: state.pending_transcript_compaction,
                transcript_compaction: state.transcript_compaction,
                effective_agent_instructions: effective_skill_state.agent_instructions,
                effective_tool_names: effective_skill_state.tool_names,
                effective_tool_catalog_digest: effective_skill_state.tool_catalog_digest,
                effective_skill_binding_digest: effective_skill_state.skill_binding_digest,
                pending_tool_calls: state.pending_tool_calls.into(),
                outstanding_tool_calls: state.outstanding_tool_calls.into_iter().collect(),
                started_tool_calls: ambiguous_started_tool_calls,
                ordered_tool_commit_queue: state.ordered_tool_commit_queue,
                staged_ordered_tool_results: state.staged_ordered_tool_results,
                recovery_replanned_tools: HashMap::new(),
                rebound_approval_event: None,
                pending_approval: state.pending_approval,
                pending_mcp_input: state.pending_mcp_input,
                resolved_mcp_input: state.resolved_mcp_input,
                pending_subagents: if state.pending_subagents.is_empty() {
                    state.pending_subagent.into_iter().collect()
                } else {
                    state.pending_subagents
                },
                active_subagents: state.active_subagents,
                completed_subagents: state.completed_subagents,
                subagent_handles: state.subagent_handles,
                subagent_message_sequences: state.subagent_message_sequences,
                subagent_message_receipts: state.subagent_message_receipts,
                subagent_message_queues: state.subagent_message_queues,
                subagent_conversations: state.subagent_conversations,
                subagent_activation_sequences: state.subagent_activation_sequences,
                subagent_generations: state.subagent_generations,
                subagent_fork_receipts: state.subagent_fork_receipts,
                subagent_archived_turns: state.subagent_archived_turns,
                subagent_generation_heads: state.subagent_generation_heads,
                subagent_rollback_receipts: state.subagent_rollback_receipts,
                subagent_budget_reservations: state.subagent_budget_reservations,
                closed_subagents: state.closed_subagents,
                subagent_result_receipts: HashMap::new(),
                steering_receipts: state.steering_receipts.into_iter().collect(),
                budget_usage: state.budget_usage,
                execution_time: ExecutionTimeBudget::restore(state.execution_time, restored_at),
                pending_budget_exhaustion: state.pending_budget_exhaustion,
                approval_decisions: HashMap::new(),
                applied_approval_decisions: state.applied_approval_decisions.into_iter().collect(),
                // A restored Run rediscovers rather than inheriting: the
                // digest it froze is recomputed against the catalog digest in
                // the checkpoint, so a server that moved while the Run was
                // suspended is caught rather than assumed unchanged.
                federated_registry: None,
                federated_executors: FederatedExecutors::default(),
                federated_definitions: Vec::new(),
                federated_tool_bindings: state.federated_tool_bindings,
                expected_federated_tool_bindings,
                federated_discovery_policy: restored_federated_discovery_policy,
                expected_federated_discovery_policy,
                runtime_mcp_read_servers: BTreeMap::new(),
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
        if let Some(pending) = execution.pending_mcp_input.as_ref() {
            return Ok(WorkerRecoveryAction::WaitForMcpInput(pending.clone()));
        }
        if !execution.pending_subagents.is_empty() {
            return Ok(WorkerRecoveryAction::WaitForSubagent);
        }
        if let Some(resolved) = execution.resolved_mcp_input.as_ref() {
            let request = execution
                .outstanding_tool_calls
                .get(&resolved.pending.tool_call_id)
                .filter(|request| request.binding_digest == resolved.pending.binding_digest)
                .cloned()
                .ok_or_else(|| {
                    WorkerAssignmentError::InvalidCheckpoint(
                        "resolved MCP input has no matching Tool execution".into(),
                    )
                })?;
            if resolved.continuation_started.is_none()
                || matches!(request.effect, ToolEffect::Pure | ToolEffect::Idempotent)
            {
                return Ok(WorkerRecoveryAction::ResumeMcpTool {
                    request,
                    pending: resolved.pending.clone(),
                    continuation: resolved.continuation.clone(),
                    dispatch_started: resolved.continuation_started.is_some(),
                });
            }
        }
        if let Some(uncertainty) = tool_outcome_uncertainty(execution)? {
            return Ok(WorkerRecoveryAction::TerminateIndeterminate(uncertainty));
        }
        if !execution.ordered_tool_commit_queue.is_empty() {
            let retry = execution
                .ordered_tool_commit_queue
                .iter()
                .filter_map(|tool_call_id| {
                    execution.outstanding_tool_calls.get(tool_call_id).cloned()
                })
                .collect::<Vec<_>>();
            if retry.is_empty() {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "ordered Tool batch has no unfinished prefix".into(),
                ));
            }
            return Ok(WorkerRecoveryAction::RetryToolBatch(retry));
        }
        if let Some(request) = execution.outstanding_tool_calls.values().next().cloned() {
            if execution.outstanding_tool_calls.len() > 1 {
                return Err(WorkerAssignmentError::InvalidCheckpoint(
                    "serial worker checkpoint contains multiple outstanding tools".into(),
                ));
            }
            return Ok(WorkerRecoveryAction::RetryTool(request));
        }
        if !execution.pending_tool_calls.is_empty() {
            return Ok(WorkerRecoveryAction::PlanPendingTool);
        }
        Ok(WorkerRecoveryAction::InvokeModel)
    }

    /// The exact child request a restored serial executor still owes. Callers
    /// receive a clone so they cannot mutate the Checkpoint-owned binding.
    pub fn pending_subagent_request(
        &self,
        attempt_id: Uuid,
    ) -> Result<Option<SubagentSpawnRequest>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.pending_subagents.first().cloned())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn pending_subagent_requests(
        &self,
        attempt_id: Uuid,
    ) -> Result<Vec<SubagentSpawnRequest>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.pending_subagents.clone())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn record_subagent_spawned(
        &mut self,
        attempt_id: Uuid,
        request: &SubagentSpawnRequest,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request_index = execution
            .pending_subagents
            .iter()
            .position(|pending| {
                pending.tool_call_id == request.tool_call_id
                    && pending.delegation_id == request.delegation_id
                    && pending.binding_digest == request.binding_digest
                    && pending.mode == SubagentSpawnMode::Async
            })
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        if execution
            .active_subagents
            .contains_key(&request.delegation_id)
            || execution
                .completed_subagents
                .contains_key(&request.delegation_id)
        {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        let event = execution
            .machine
            .record_subagent_spawned(request)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.pending_subagents.remove(request_index);
        execution
            .active_subagents
            .insert(request.delegation_id, request.clone());
        execution
            .subagent_handles
            .insert(request.delegation_id, request.clone());
        execution
            .subagent_message_sequences
            .entry(request.delegation_id)
            .or_insert(0);
        execution
            .subagent_conversations
            .insert(request.delegation_id, Vec::new());
        execution
            .subagent_activation_sequences
            .insert(request.delegation_id, 0);
        execution
            .subagent_generations
            .insert(request.delegation_id, 1);
        let content = serde_json::json!({
            "agent_id": request.delegation_id,
            "generation": 1,
            "role": request.role,
            "status": "running"
        });
        execution.transcript.push(ModelMessage {
            role: ModelRole::Tool as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::ToolResult(ToolResultPart {
                    tool_call_id: request.tool_call_id.clone(),
                    content_json: serde_json::to_vec(&content)
                        .expect("subagent handle result is serializable"),
                })),
            }],
        });
        Ok(event)
    }

    pub fn active_subagent_request(
        &self,
        attempt_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<SubagentSpawnRequest>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.active_subagents.get(&agent_id).cloned())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn active_subagent_requests(
        &self,
        attempt_id: Uuid,
    ) -> Result<Vec<(Uuid, SubagentSpawnRequest)>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| {
                execution
                    .active_subagents
                    .iter()
                    .map(|(agent_id, request)| (*agent_id, request.clone()))
                    .collect()
            })
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn completed_subagent_result(
        &self,
        attempt_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<SubagentResultDelivery>, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.completed_subagents.get(&agent_id).cloned())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    pub fn subagent_history(
        &self,
        attempt_id: Uuid,
        agent_id: Uuid,
        after_activation_ordinal: Option<u64>,
        limit: u16,
    ) -> Result<SubagentHistoryPage, WorkerAssignmentError> {
        let generation = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?
            .subagent_generations
            .get(&agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        self.subagent_history_at_generation(
            attempt_id,
            agent_id,
            generation,
            after_activation_ordinal,
            limit,
        )
    }

    pub fn subagent_history_at_generation(
        &self,
        attempt_id: Uuid,
        agent_id: Uuid,
        generation: u64,
        after_activation_ordinal: Option<u64>,
        limit: u16,
    ) -> Result<SubagentHistoryPage, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if limit == 0 || limit > 50 || !execution.subagent_handles.contains_key(&agent_id) {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let current_generation = execution
            .subagent_generations
            .get(&agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        if generation == 0 || generation > current_generation {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let history = materialize_subagent_generation(execution, agent_id, generation)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let mut selected = history
            .iter()
            .filter(|turn| {
                after_activation_ordinal
                    .map(|after| turn.activation_ordinal > after)
                    .unwrap_or(true)
            })
            .take(usize::from(limit).saturating_add(1))
            .cloned()
            .collect::<Vec<_>>();
        let has_more = selected.len() > usize::from(limit);
        if has_more {
            selected.pop();
        }
        let next_after_activation_ordinal = selected.last().map(|turn| turn.activation_ordinal);
        let closed = execution.closed_subagents.contains(&agent_id);
        let status = if generation != current_generation {
            "archived"
        } else if closed {
            "closed"
        } else if execution.active_subagents.contains_key(&agent_id) {
            "running"
        } else if execution.completed_subagents.contains_key(&agent_id) {
            "terminal"
        } else {
            "unknown"
        };
        Ok(SubagentHistoryPage {
            agent_id,
            generation,
            forked_from: execution
                .subagent_fork_receipts
                .values()
                .find(|record| record.receipt.agent_id == agent_id)
                .map(|record| record.receipt.clone()),
            turns: selected,
            next_after_activation_ordinal,
            has_more,
            status: status.into(),
            queued_messages: if generation == current_generation {
                execution
                    .subagent_message_queues
                    .get(&agent_id)
                    .map_or(0, VecDeque::len)
            } else {
                0
            },
            closed,
        })
    }

    pub fn fork_async_subagent(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
        tool_binding_digest: &str,
    ) -> Result<SubagentForkOutcome, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request = execution
            .outstanding_tool_calls
            .get(tool_call_id)
            .filter(|request| {
                request.call.name == "agent.fork" && request.binding_digest == tool_binding_digest
            })
            .cloned()
            .ok_or(WorkerAssignmentError::ToolResultBindingMismatch)?;
        if let Some(record) = execution.subagent_fork_receipts.get(tool_call_id) {
            if record.receipt.tool_binding_digest != tool_binding_digest {
                return Err(WorkerAssignmentError::ToolResultBindingMismatch);
            }
            return Ok(SubagentForkOutcome {
                receipt: record.receipt.clone(),
                event: record.event.clone(),
                created: false,
            });
        }
        if execution.subagent_handles.len() >= 64 {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let arguments: SubagentForkArguments = serde_json::from_value(request.call.arguments)
            .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
        let source_handle = execution
            .subagent_handles
            .get(&arguments.source_agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let source_generation = execution
            .subagent_generations
            .get(&arguments.source_agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        if source_generation != arguments.source_generation {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let source_history = execution
            .subagent_conversations
            .get(&arguments.source_agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let boundary_index = source_history
            .iter()
            .position(|turn| turn.activation_ordinal == arguments.through_activation_ordinal)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let fork_history = source_history[..=boundary_index].to_vec();
        let last = fork_history
            .last()
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let reserved = subagent_budget_reservation_totals(&execution.subagent_budget_reservations);
        let remaining_tokens = execution
            .command
            .budget
            .max_tokens
            .saturating_sub(execution.budget_usage.tokens)
            .saturating_sub(reserved.max_tokens);
        let used_cost_cents = execution.budget_usage.cost_micros.saturating_add(9_999) / 10_000;
        let remaining_cost_cents = execution
            .command
            .budget
            .max_cost_cents
            .saturating_sub(used_cost_cents)
            .saturating_sub(reserved.max_cost_cents);
        let remaining_duration = execution
            .execution_time
            .remaining(execution.command.budget.max_duration_seconds);
        let remaining_duration_seconds =
            u64::try_from(remaining_duration.as_millis().saturating_add(999) / 1_000)
                .unwrap_or(u64::MAX)
                .saturating_sub(reserved.max_duration_seconds);
        let budget = agent_protocol::RunBudget {
            max_tokens: arguments.max_tokens,
            max_cost_cents: arguments.max_cost_cents,
            max_duration_seconds: arguments.max_duration_seconds,
        };
        if budget.max_tokens == 0
            || budget.max_tokens > source_handle.budget.max_tokens
            || budget.max_tokens > remaining_tokens
            || budget.max_cost_cents == 0
            || budget.max_cost_cents > source_handle.budget.max_cost_cents
            || budget.max_cost_cents > remaining_cost_cents
            || budget.max_duration_seconds == 0
            || budget.max_duration_seconds > source_handle.budget.max_duration_seconds
            || budget.max_duration_seconds > remaining_duration_seconds
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let agent_id = deterministic_subagent_fork_id(
            &execution.command,
            tool_call_id,
            arguments.source_agent_id,
            arguments.through_activation_ordinal,
        );
        if execution.subagent_handles.contains_key(&agent_id) {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let fork_handle = SubagentSpawnRequest {
            tool_call_id: last.result.tool_call_id.clone(),
            delegation_id: last.child_run_id,
            role: source_handle.role.clone(),
            input: last.input.clone(),
            budget: budget.clone(),
            binding_digest: last.result.binding_digest.clone(),
            mode: SubagentSpawnMode::Async,
            conversation_history: fork_history[..fork_history.len() - 1].to_vec(),
        };
        if !fork_handle.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let receipt = SubagentForkReceipt {
            tool_call_id: tool_call_id.to_owned(),
            tool_binding_digest: tool_binding_digest.to_owned(),
            source_agent_id: arguments.source_agent_id,
            source_generation,
            through_activation_ordinal: arguments.through_activation_ordinal,
            source_history_digest: agent_protocol::subagent_conversation_history_digest(
                &fork_history,
            ),
            agent_id,
            generation: 1,
            role: source_handle.role,
            budget,
        };
        if !receipt.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let event = execution
            .machine
            .record_subagent_forked(&receipt)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.subagent_handles.insert(agent_id, fork_handle);
        execution.subagent_message_sequences.insert(
            agent_id,
            fork_history
                .iter()
                .map(|turn| turn.message_sequence)
                .max()
                .unwrap_or(0),
        );
        execution
            .subagent_message_receipts
            .insert(agent_id, BTreeMap::new());
        execution
            .subagent_message_queues
            .insert(agent_id, VecDeque::new());
        execution
            .subagent_conversations
            .insert(agent_id, fork_history);
        execution
            .subagent_activation_sequences
            .insert(agent_id, arguments.through_activation_ordinal);
        execution.subagent_generations.insert(agent_id, 1);
        execution.completed_subagents.insert(agent_id, last.result);
        execution.subagent_fork_receipts.insert(
            tool_call_id.to_owned(),
            SubagentForkRecord {
                receipt: receipt.clone(),
                event: event.clone(),
            },
        );
        Ok(SubagentForkOutcome {
            receipt,
            event,
            created: true,
        })
    }

    pub fn rollback_async_subagent(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
        tool_binding_digest: &str,
    ) -> Result<SubagentRollbackOutcome, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request = execution
            .outstanding_tool_calls
            .get(tool_call_id)
            .filter(|request| {
                request.call.name == "agent.rollback"
                    && request.binding_digest == tool_binding_digest
            })
            .cloned()
            .ok_or(WorkerAssignmentError::ToolResultBindingMismatch)?;
        if let Some(record) = execution.subagent_rollback_receipts.get(tool_call_id) {
            if record.receipt.tool_binding_digest != tool_binding_digest {
                return Err(WorkerAssignmentError::ToolResultBindingMismatch);
            }
            return Ok(SubagentRollbackOutcome {
                receipt: record.receipt.clone(),
                event: record.event.clone(),
                created: false,
            });
        }
        let arguments: SubagentRollbackArguments =
            serde_json::from_value(request.call.arguments)
                .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
        let current_generation = execution
            .subagent_generations
            .get(&arguments.agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let generation = current_generation
            .checked_add(1)
            .filter(|generation| *generation <= SUBAGENT_MAX_GENERATIONS)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        if current_generation != arguments.generation
            || execution.active_subagents.contains_key(&arguments.agent_id)
            || execution.closed_subagents.contains(&arguments.agent_id)
            || !execution
                .completed_subagents
                .contains_key(&arguments.agent_id)
            || execution
                .subagent_message_queues
                .get(&arguments.agent_id)
                .is_some_and(|queue| !queue.is_empty())
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let current_history = execution
            .subagent_conversations
            .get(&arguments.agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let boundary_index = current_history
            .iter()
            .position(|turn| turn.activation_ordinal == arguments.through_activation_ordinal)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        if boundary_index + 1 >= current_history.len() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let restored_history = current_history[..=boundary_index].to_vec();
        let superseded_suffix = &current_history[boundary_index + 1..];
        let mut archived_turns = execution
            .subagent_archived_turns
            .get(&arguments.agent_id)
            .cloned()
            .unwrap_or_default();
        for turn in superseded_suffix {
            if archived_turns
                .insert(turn.activation_ordinal, turn.clone())
                .is_some()
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
        }
        if archived_turns.len() > SUBAGENT_ARCHIVE_MAX_TURNS
            || serde_json::to_vec(&archived_turns)
                .map_or(true, |encoded| encoded.len() > SUBAGENT_ARCHIVE_MAX_BYTES)
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let previous_history_digest =
            agent_protocol::subagent_conversation_history_digest(&current_history);
        let restored_history_digest =
            agent_protocol::subagent_conversation_history_digest(&restored_history);
        let mut generation_heads = execution
            .subagent_generation_heads
            .get(&arguments.agent_id)
            .cloned()
            .unwrap_or_default();
        if generation_heads
            .insert(
                current_generation,
                SubagentGenerationHead {
                    activation_ordinals: current_history
                        .iter()
                        .map(|turn| turn.activation_ordinal)
                        .collect(),
                    history_digest: previous_history_digest.clone(),
                },
            )
            .is_some()
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let current_handle = execution
            .subagent_handles
            .get(&arguments.agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let last = restored_history
            .last()
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let restored_handle = SubagentSpawnRequest {
            tool_call_id: last.result.tool_call_id.clone(),
            delegation_id: last.child_run_id,
            role: current_handle.role,
            input: last.input.clone(),
            budget: current_handle.budget,
            binding_digest: last.result.binding_digest.clone(),
            mode: SubagentSpawnMode::Async,
            conversation_history: restored_history[..restored_history.len() - 1].to_vec(),
        };
        if !restored_handle.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let receipt = SubagentRollbackReceipt {
            tool_call_id: tool_call_id.to_owned(),
            tool_binding_digest: tool_binding_digest.to_owned(),
            agent_id: arguments.agent_id,
            from_generation: current_generation,
            generation,
            through_activation_ordinal: arguments.through_activation_ordinal,
            previous_history_digest,
            restored_history_digest,
        };
        if !receipt.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let event = execution
            .machine
            .record_subagent_rolled_back(&receipt)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution
            .subagent_archived_turns
            .insert(arguments.agent_id, archived_turns);
        execution
            .subagent_generation_heads
            .insert(arguments.agent_id, generation_heads);
        execution
            .subagent_conversations
            .insert(arguments.agent_id, restored_history);
        execution
            .subagent_generations
            .insert(arguments.agent_id, generation);
        execution
            .subagent_handles
            .insert(arguments.agent_id, restored_handle);
        execution
            .completed_subagents
            .insert(arguments.agent_id, last.result);
        execution.subagent_rollback_receipts.insert(
            tool_call_id.to_owned(),
            SubagentRollbackRecord {
                receipt: receipt.clone(),
                event: event.clone(),
            },
        );
        Ok(SubagentRollbackOutcome {
            receipt,
            event,
            created: true,
        })
    }

    pub fn record_async_subagent_closed(
        &mut self,
        attempt_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<EventEnvelope>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if !execution.subagent_handles.contains_key(&agent_id)
            || execution.active_subagents.contains_key(&agent_id)
            || !execution.completed_subagents.contains_key(&agent_id)
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        if !execution.closed_subagents.insert(agent_id) {
            return Ok(None);
        }
        if let Some(queue) = execution.subagent_message_queues.remove(&agent_id)
            && let Some(receipts) = execution.subagent_message_receipts.get_mut(&agent_id)
        {
            let mut cancelled_requests = Vec::with_capacity(queue.len());
            for key in queue {
                if let Some(receipt) = receipts.get_mut(&key) {
                    receipt.status = SubagentMessageStatus::Cancelled;
                    cancelled_requests.push(receipt.child_request.clone());
                }
            }
            for request in cancelled_requests {
                remove_subagent_budget_reservation(
                    &mut execution.subagent_budget_reservations,
                    agent_id,
                    &request,
                )?;
            }
        }
        execution
            .machine
            .record_subagent_closed(agent_id)
            .map(Some)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
    }

    pub fn continue_async_subagent(
        &mut self,
        attempt_id: Uuid,
        agent_id: Uuid,
        idempotency_key: &str,
        message: &str,
    ) -> Result<AsyncSubagentContinuation, WorkerAssignmentError> {
        let generation = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?
            .subagent_generations
            .get(&agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        self.continue_async_subagent_at_generation(
            attempt_id,
            agent_id,
            generation,
            idempotency_key,
            message,
        )
    }

    pub fn continue_async_subagent_at_generation(
        &mut self,
        attempt_id: Uuid,
        agent_id: Uuid,
        generation: u64,
        idempotency_key: &str,
        message: &str,
    ) -> Result<AsyncSubagentContinuation, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.subagent_generations.get(&agent_id) != Some(&generation) {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let message_digest = digest_bytes(message.as_bytes());
        let interrupt = execution
            .outstanding_tool_calls
            .values()
            .filter(|request| request.call.name == "agent.send")
            .filter_map(|request| {
                serde_json::from_value::<SubagentSendArguments>(request.call.arguments.clone()).ok()
            })
            .find(|arguments| {
                arguments.agent_id == agent_id
                    && arguments.idempotency_key == idempotency_key
                    && arguments.message == message
            })
            .is_some_and(|arguments| arguments.interrupt);
        if let Some(receipt) = execution
            .subagent_message_receipts
            .get(&agent_id)
            .and_then(|receipts| receipts.get(idempotency_key))
            .cloned()
        {
            if receipt.message_digest != message_digest || receipt.interrupt != interrupt {
                return Err(WorkerAssignmentError::SubagentMessageConflict);
            }
            let active_request = execution
                .active_subagents
                .get(&agent_id)
                .filter(|active| active.binding_digest == receipt.child_request.binding_digest)
                .cloned();
            return Ok(AsyncSubagentContinuation {
                receipt,
                accepted_event: None,
                active_request,
            });
        }
        if message.trim().is_empty()
            || message.len() > 32_000
            || !valid_subagent_message_idempotency_key(idempotency_key)
            || (!execution.active_subagents.contains_key(&agent_id)
                && !execution.completed_subagents.contains_key(&agent_id))
            || execution.closed_subagents.contains(&agent_id)
            || execution
                .subagent_message_queues
                .get(&agent_id)
                .is_some_and(|queue| queue.len() >= 8)
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let template = execution
            .subagent_handles
            .get(&agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let message_sequence = execution
            .subagent_message_sequences
            .get(&agent_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let reserved = subagent_budget_reservation_totals(&execution.subagent_budget_reservations);
        let remaining_tokens = execution
            .command
            .budget
            .max_tokens
            .saturating_sub(execution.budget_usage.tokens)
            .saturating_sub(reserved.max_tokens);
        let used_cost_cents = execution.budget_usage.cost_micros.saturating_add(9_999) / 10_000;
        let remaining_cost_cents = execution
            .command
            .budget
            .max_cost_cents
            .saturating_sub(used_cost_cents)
            .saturating_sub(reserved.max_cost_cents);
        let remaining_duration = execution
            .execution_time
            .remaining(execution.command.budget.max_duration_seconds);
        let remaining_duration_seconds =
            u64::try_from(remaining_duration.as_millis().saturating_add(999) / 1_000)
                .unwrap_or(u64::MAX)
                .saturating_sub(reserved.max_duration_seconds);
        let budget = agent_protocol::RunBudget {
            max_tokens: template.budget.max_tokens.min(remaining_tokens),
            max_cost_cents: template.budget.max_cost_cents.min(remaining_cost_cents),
            max_duration_seconds: template
                .budget
                .max_duration_seconds
                .min(remaining_duration_seconds),
        };
        if budget.max_tokens == 0 || budget.max_cost_cents == 0 || budget.max_duration_seconds == 0
        {
            return Err(WorkerAssignmentError::BudgetExhausted);
        }
        let child_run_id =
            deterministic_subagent_turn_id(&execution.command, agent_id, message_sequence);
        let tool_call_id = format!("agent.send:{agent_id}:{message_sequence}");
        let starts_now = !execution.active_subagents.contains_key(&agent_id);
        let current_history = execution
            .subagent_conversations
            .get(&agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        // A queued receipt is an accepted intent, not yet an execution
        // binding. Keeping the current prefix here would duplicate up to 2 MiB
        // eight times and would still be stale after the active turn ends.
        // Activation replaces this empty prefix and recomputes the digest.
        let conversation_history = if starts_now {
            current_history
        } else {
            Vec::new()
        };
        let binding_digest = subagent_continuation_binding_digest(SubagentContinuationBinding {
            command: &execution.command,
            agent_id,
            generation: execution
                .subagent_generations
                .get(&agent_id)
                .copied()
                .ok_or(WorkerAssignmentError::InvalidToolCall)?,
            idempotency_key,
            message_sequence,
            child_run_id,
            role: &template.role,
            message,
            budget: &budget,
            conversation_history: &conversation_history,
        });
        let request = SubagentSpawnRequest {
            tool_call_id,
            delegation_id: child_run_id,
            role: template.role,
            input: message.to_owned(),
            budget,
            binding_digest,
            mode: SubagentSpawnMode::Async,
            conversation_history,
        };
        if !request.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let submission_id = format!("{agent_id}:{message_sequence}");
        let status = if starts_now { "running" } else { "queued" };
        let event = execution
            .machine
            .record_subagent_input_accepted(SubagentInputAcceptance {
                agent_id,
                message_sequence,
                idempotency_key,
                submission_id: &submission_id,
                status,
                interrupt,
                request: &request,
            })
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        insert_subagent_budget_reservation(
            &mut execution.subagent_budget_reservations,
            agent_id,
            &request,
        )?;
        let receipt = SubagentMessageReceipt {
            agent_id,
            idempotency_key: idempotency_key.to_owned(),
            message_digest,
            message_sequence,
            submission_id,
            interrupt,
            status: if starts_now {
                SubagentMessageStatus::Active
            } else {
                SubagentMessageStatus::Queued
            },
            child_request: request.clone(),
        };
        if starts_now {
            let activation_ordinal = execution
                .subagent_activation_sequences
                .get(&agent_id)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            execution
                .subagent_activation_sequences
                .insert(agent_id, activation_ordinal);
            execution.completed_subagents.remove(&agent_id);
            execution.active_subagents.insert(agent_id, request.clone());
            execution.subagent_handles.insert(agent_id, request.clone());
        } else {
            let queue = execution
                .subagent_message_queues
                .entry(agent_id)
                .or_default();
            if interrupt {
                queue.push_front(idempotency_key.to_owned());
            } else {
                queue.push_back(idempotency_key.to_owned());
            }
        }
        execution
            .subagent_message_sequences
            .insert(agent_id, message_sequence);
        execution
            .subagent_message_receipts
            .entry(agent_id)
            .or_default()
            .insert(idempotency_key.to_owned(), receipt.clone());
        Ok(AsyncSubagentContinuation {
            receipt,
            accepted_event: Some(event),
            active_request: starts_now.then_some(request),
        })
    }

    pub fn record_async_subagent_result(
        &mut self,
        attempt_id: Uuid,
        agent_id: Uuid,
        result: &SubagentResultDelivery,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request = execution
            .active_subagents
            .get(&agent_id)
            .filter(|request| {
                request.mode == SubagentSpawnMode::Async
                    && request.tool_call_id == result.tool_call_id
                    && request.binding_digest == result.binding_digest
            })
            .cloned()
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        if !result.is_well_formed() || result.child_run_id != request.delegation_id {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        let message_sequence = execution
            .subagent_message_receipts
            .get(&agent_id)
            .and_then(|receipts| {
                receipts
                    .values()
                    .find(|receipt| receipt.child_request.binding_digest == request.binding_digest)
            })
            .map_or(0, |receipt| receipt.message_sequence);
        let activation_ordinal = execution
            .subagent_activation_sequences
            .get(&agent_id)
            .copied()
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        let mut conversation = execution
            .subagent_conversations
            .get(&agent_id)
            .cloned()
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        if request.conversation_history != conversation {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        conversation.push(SubagentConversationTurn {
            activation_ordinal,
            message_sequence,
            child_run_id: request.delegation_id,
            input: request.input.clone(),
            result: result.clone(),
        });
        if !agent_protocol::subagent_conversation_history_is_well_formed(&conversation) {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        let event = execution
            .machine
            .record_subagent_terminal_observed(result)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        remove_subagent_budget_reservation(
            &mut execution.subagent_budget_reservations,
            agent_id,
            &request,
        )?;
        execution.active_subagents.remove(&agent_id);
        execution
            .completed_subagents
            .insert(agent_id, result.clone());
        execution
            .subagent_conversations
            .insert(agent_id, conversation);
        if let Some(receipts) = execution.subagent_message_receipts.get_mut(&agent_id)
            && let Some(receipt) = receipts
                .values_mut()
                .find(|receipt| receipt.child_request.binding_digest == request.binding_digest)
        {
            receipt.status = SubagentMessageStatus::Completed;
        }
        execution.budget_usage.tokens = execution
            .budget_usage
            .tokens
            .saturating_add(result.usage.tokens);
        execution.budget_usage.cost_micros = execution
            .budget_usage
            .cost_micros
            .saturating_add(result.usage.cost_micros);
        execution.pending_budget_exhaustion =
            budget_exhaustion(execution.budget_usage, &execution.command.budget);
        Ok(event)
    }

    pub fn activate_next_subagent_message(
        &mut self,
        attempt_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<SubagentMessageActivation>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.active_subagents.contains_key(&agent_id) {
            return Ok(None);
        }
        if execution.closed_subagents.contains(&agent_id)
            || !execution.completed_subagents.contains_key(&agent_id)
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        let Some(idempotency_key) = execution
            .subagent_message_queues
            .get_mut(&agent_id)
            .and_then(VecDeque::pop_front)
        else {
            return Ok(None);
        };
        let queued_receipt = execution
            .subagent_message_receipts
            .get(&agent_id)
            .and_then(|receipts| receipts.get(&idempotency_key))
            .cloned()
            .ok_or_else(|| {
                WorkerAssignmentError::InvalidCheckpoint(
                    "subagent mailbox key has no durable receipt".into(),
                )
            })?;
        if queued_receipt.status != SubagentMessageStatus::Queued {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "subagent mailbox receipt is not queued".into(),
            ));
        }
        let conversation_history = execution
            .subagent_conversations
            .get(&agent_id)
            .cloned()
            .ok_or_else(|| {
                WorkerAssignmentError::InvalidCheckpoint(
                    "subagent mailbox has no durable conversation".into(),
                )
            })?;
        let mut request = queued_receipt.child_request.clone();
        request.conversation_history = conversation_history;
        request.binding_digest =
            subagent_continuation_binding_digest(SubagentContinuationBinding {
                command: &execution.command,
                agent_id,
                generation: execution
                    .subagent_generations
                    .get(&agent_id)
                    .copied()
                    .ok_or_else(|| {
                        WorkerAssignmentError::InvalidCheckpoint(
                            "subagent mailbox has no durable generation".into(),
                        )
                    })?,
                idempotency_key: &queued_receipt.idempotency_key,
                message_sequence: queued_receipt.message_sequence,
                child_run_id: request.delegation_id,
                role: &request.role,
                message: &request.input,
                budget: &request.budget,
                conversation_history: &request.conversation_history,
            });
        if !request.is_well_formed() {
            return Err(WorkerAssignmentError::InvalidCheckpoint(
                "activated subagent request is malformed".into(),
            ));
        }
        let activation_ordinal = execution
            .subagent_activation_sequences
            .get(&agent_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(WorkerAssignmentError::InvalidToolCall)?;
        let event = execution
            .machine
            .record_subagent_input_activated(agent_id, queued_receipt.message_sequence, &request)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        rebind_subagent_budget_reservation(
            &mut execution.subagent_budget_reservations,
            agent_id,
            &queued_receipt.child_request,
            &request,
        )?;
        let receipt = execution
            .subagent_message_receipts
            .get_mut(&agent_id)
            .and_then(|receipts| receipts.get_mut(&idempotency_key))
            .expect("queued receipt was read from this map");
        receipt.status = SubagentMessageStatus::Active;
        receipt.child_request = request.clone();
        execution
            .subagent_activation_sequences
            .insert(agent_id, activation_ordinal);
        execution.completed_subagents.remove(&agent_id);
        execution.active_subagents.insert(agent_id, request.clone());
        execution.subagent_handles.insert(agent_id, request.clone());
        let receipt = receipt.clone();
        Ok(Some(SubagentMessageActivation {
            receipt,
            event,
            request,
        }))
    }

    pub fn pending_subagent_interrupts(
        &self,
        attempt_id: Uuid,
    ) -> Result<Vec<Uuid>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        Ok(execution
            .subagent_message_queues
            .iter()
            .filter_map(|(agent_id, queue)| {
                let key = queue.front()?;
                let receipt = execution
                    .subagent_message_receipts
                    .get(agent_id)?
                    .get(key)?;
                (receipt.interrupt && execution.active_subagents.contains_key(agent_id))
                    .then_some(*agent_id)
            })
            .collect())
    }

    /// Whether the next Tool call belongs to the current adjacent subagent
    /// batch. Hosts use this to make every spawn intent durable before any
    /// child starts, while an ordinary Tool remains an ordering barrier.
    pub fn next_pending_tool_is_subagent(
        &self,
        attempt_id: Uuid,
    ) -> Result<bool, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| {
                execution
                    .pending_tool_calls
                    .front()
                    .is_some_and(|call| call.name == "agent.spawn")
            })
            .ok_or(WorkerAssignmentError::UnknownAttempt)
    }

    /// Counts the adjacent prefix whose declared effect is `Pure`. Approval is
    /// deliberately not consumed here: the Host still performs each policy
    /// decision sequentially before starting any member of the batch.
    pub fn pending_parallel_safe_tool_prefix_len(
        &self,
        attempt_id: Uuid,
        limit: usize,
    ) -> Result<usize, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if limit < 2 || execution.pending_approval.is_some() {
            return Ok(0);
        }
        let registry = execution
            .federated_registry
            .as_ref()
            .unwrap_or(&self.tool_registry);
        let mut count = 0;
        for call in execution.pending_tool_calls.iter().take(limit) {
            let runtime_mcp_read = mcp_gateway::is_runtime_mcp_read_tool(&call.name);
            if call.name.starts_with("agent.")
                || (!runtime_mcp_read && !execution.effective_tool_names.contains(&call.name))
                || (runtime_mcp_read
                    && !mcp_gateway::runtime_mcp_read_call_is_authorized(
                        &call.name,
                        &call.arguments,
                        &execution.runtime_mcp_read_servers,
                        &execution.command.delegated_scopes,
                    ))
            {
                break;
            }
            let descriptor =
                match registry.authorize(&call.name, &execution.command.delegated_scopes) {
                    Ok(descriptor) => descriptor,
                    Err(_) => break,
                };
            if descriptor.effect != ToolEffect::Pure || descriptor.approval == ApprovalMode::Deny {
                break;
            }
            count += 1;
        }
        Ok(count)
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
        execution.execution_time.pause();
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
                // A federated tool is not installed on this Worker and never
                // will be -- it is discovered per Run (ADR-0040). Judging it by
                // `tool_definitions` rejects the whole Run for declaring
                // something perfectly legitimate.
                //
                // It is effective when the command carries its server and the
                // AgentVersion delegated that server's scope. Both are checked
                // here rather than assumed, so a Skill still cannot widen
                // anything: declaring `mcp:other/x` with no such server, or
                // without `tool:mcp:other`, is refused exactly as an unavailable
                // native tool is.
                if let Some(server_name) = federated_server_of(tool_name) {
                    let scope = format!("tool:mcp:{server_name}");
                    if !command.delegated_scopes.contains(&scope)
                        || !command
                            .mcp_servers
                            .iter()
                            .any(|server| server.name == server_name)
                    {
                        return Err(WorkerAssignmentError::ToolConfiguration(format!(
                            "Skill {} requires federated tool {tool_name} whose server is not \
                             registered for this run or whose scope is not delegated",
                            skill.name
                        )));
                    }
                    declared_tool_names.insert(tool_name.clone());
                    tool_names.insert(tool_name.clone());
                    continue;
                }
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

    pub fn record_required_mcp_unavailable(
        &mut self,
        attempt_id: Uuid,
        server_names: &[String],
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        if server_names.is_empty()
            || server_names
                .iter()
                .any(|name| name.trim().is_empty() || name.len() > 128)
        {
            return Err(WorkerAssignmentError::ToolConfiguration(
                "required MCP failure has no valid bound server identity".into(),
            ));
        }
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
        let event = execution
            .machine
            .record_required_mcp_unavailable(server_names)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.cancellation.cancel();
        execution.subagent_budget_reservations.clear();
        execution.terminal_event = Some(event.clone());
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
        match &model_event {
            ModelStreamEvent::Reasoning {
                summary,
                private_state,
            } if summary.len() > 64
                || summary.iter().any(|item| item.len() > 32_000)
                || private_state
                    .as_ref()
                    .is_some_and(|state| !state.is_well_formed()) =>
            {
                return Err(WorkerAssignmentError::InvalidTranscript(
                    "model reasoning item is malformed or exceeds its bound".into(),
                ));
            }
            ModelStreamEvent::Refusal { text }
                if text.trim().is_empty() || text.len() > 128_000 =>
            {
                return Err(WorkerAssignmentError::InvalidTranscript(
                    "model refusal is blank or exceeds its bound".into(),
                ));
            }
            ModelStreamEvent::PrivateStateOmitted {
                origin_provider_id,
                target_provider_id,
                format,
            } if origin_provider_id.trim().is_empty()
                || target_provider_id.trim().is_empty()
                || format.trim().is_empty()
                || origin_provider_id.len() > 128
                || target_provider_id.len() > 128
                || format.len() > 128 =>
            {
                return Err(WorkerAssignmentError::InvalidTranscript(
                    "private-state omission audit is malformed".into(),
                ));
            }
            _ => {}
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
            ModelStreamEvent::TextDelta { text } => {
                execution.assistant_text_buffer.push_str(&text);
            }
            ModelStreamEvent::Reasoning {
                summary,
                private_state,
            } => {
                execution.assistant_rich_content_buffer.push(ContentPart {
                    body: Some(content_part::Body::Reasoning(ReasoningPart {
                        summary,
                        private_state: private_state.map(|state| WireProviderPrivateState {
                            provider_id: state.provider_id,
                            protocol: state.protocol,
                            model: state.model,
                            format: state.format,
                            data: state.data,
                        }),
                    })),
                });
            }
            ModelStreamEvent::Refusal { text } => {
                execution.assistant_rich_content_buffer.push(ContentPart {
                    body: Some(content_part::Body::Refusal(RefusalPart { text })),
                });
            }
            ModelStreamEvent::PrivateStateOmitted { .. } => {}
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
            } => {
                let mut content = std::mem::take(&mut execution.assistant_rich_content_buffer);
                content.reserve(
                    usize::from(!execution.assistant_text_buffer.is_empty())
                        .saturating_add(execution.pending_tool_calls.len()),
                );
                if !execution.assistant_text_buffer.is_empty() {
                    content.push(ContentPart {
                        body: Some(content_part::Body::Text(TextPart {
                            text: std::mem::take(&mut execution.assistant_text_buffer),
                        })),
                    });
                }
                content.extend(execution.pending_tool_calls.iter().map(|call| {
                    ContentPart {
                        body: Some(content_part::Body::ToolCall(ToolCallPart {
                            tool_call_id: call.id.clone(),
                            name: call.name.clone(),
                            arguments_json: serde_json::to_vec(&call.arguments)
                                .expect("tool call arguments are serializable"),
                        })),
                    }
                }));
                execution.transcript.push(ModelMessage {
                    role: ModelRole::Assistant as i32,
                    content,
                });
            }
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            } if !execution.assistant_text_buffer.is_empty()
                || !execution.assistant_rich_content_buffer.is_empty() =>
            {
                let mut content = std::mem::take(&mut execution.assistant_rich_content_buffer);
                if !execution.assistant_text_buffer.is_empty() {
                    content.push(ContentPart {
                        body: Some(content_part::Body::Text(TextPart {
                            text: std::mem::take(&mut execution.assistant_text_buffer),
                        })),
                    });
                }
                execution.transcript.push(ModelMessage {
                    role: ModelRole::Assistant as i32,
                    content,
                });
            }
            _ => {}
        }
        if execution.machine.status().is_terminal() {
            execution.cancellation.cancel();
            execution.subagent_budget_reservations.clear();
            execution.terminal_event = Some(event.clone());
        }
        Ok(event)
    }

    pub fn record_model_provider_failure(
        &mut self,
        attempt_id: Uuid,
        provider_id: &str,
        kind: ModelErrorKind,
        retryable: bool,
        status: Option<u16>,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        if provider_id.trim().is_empty() || provider_id.len() > 128 {
            return Err(WorkerAssignmentError::InvalidCommand(
                "model Provider id is invalid".into(),
            ));
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        execution
            .machine
            .record_model_provider_failure(provider_id, kind, retryable, status)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
    }

    pub fn record_model_provider_selection(
        &mut self,
        attempt_id: Uuid,
        provider_id: &str,
        failed_provider_ids: &[String],
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        if provider_id.trim().is_empty()
            || provider_id.len() > 128
            || failed_provider_ids.len() > 8
            || failed_provider_ids
                .iter()
                .any(|id| id.trim().is_empty() || id.len() > 128 || id == provider_id)
        {
            return Err(WorkerAssignmentError::InvalidCommand(
                "model Provider selection is invalid".into(),
            ));
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        execution
            .machine
            .record_model_provider_selection(provider_id, failed_provider_ids)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
    }

    pub fn record_model_provider_retry_scheduled(
        &mut self,
        attempt_id: Uuid,
        provider_id: &str,
        provider_attempt: u8,
        delay_ms: u64,
        kind: ModelErrorKind,
        status: Option<u16>,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        if provider_id.trim().is_empty()
            || provider_id.len() > 128
            || !(1..=4).contains(&provider_attempt)
            || delay_ms > 3_600_000
        {
            return Err(WorkerAssignmentError::InvalidCommand(
                "model Provider retry observation is invalid".into(),
            ));
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        execution
            .machine
            .record_model_provider_retry_scheduled(
                provider_id,
                provider_attempt,
                delay_ms,
                kind,
                status,
            )
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
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

    pub fn terminate_uncertain_tool(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        self.terminate_uncertain_tool_with_interruption(attempt_id, None)
    }

    fn terminate_uncertain_tool_with_interruption(
        &mut self,
        attempt_id: Uuid,
        interruption: Option<RunInterruption>,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let uncertainty = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)
            .and_then(tool_outcome_uncertainty)?
            .ok_or(WorkerAssignmentError::AmbiguousToolExecution)?;
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .expect("execution was checked above");
        if let Some(terminal) = &execution.terminal_event {
            return Ok(terminal.clone());
        }
        execution.cancellation.cancel();
        let mut event = execution
            .machine
            .apply(RunCommand::ToolOutcomeUnknown {
                effect: uncertainty.request.effect,
            })
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        event.payload["tool_call_id"] = serde_json::json!(uncertainty.request.call.id);
        event.payload["tool_name"] = serde_json::json!(uncertainty.request.call.name);
        event.payload["binding_digest"] = serde_json::json!(uncertainty.request.binding_digest);
        event.payload["sandbox"] = serde_json::json!(uncertainty.request.sandbox);
        event.payload["source_attempt_id"] = serde_json::json!(uncertainty.source_attempt_id);
        event.payload["started_event_id"] = serde_json::json!(uncertainty.started_event_id);
        event.payload["started_sequence"] = serde_json::json!(uncertainty.started_sequence);
        event.payload["reason"] = serde_json::json!("tool_outcome_unknown");
        if let Some(interruption) = interruption {
            event.payload["interrupted_by"] = serde_json::json!(interruption.as_str());
            event.payload["requested_status"] = serde_json::json!(interruption.requested_status());
        }
        execution.terminal_event = Some(event.clone());
        Ok(event)
    }

    fn terminate_budget_exhaustion(
        execution: &mut ActiveExecution,
        dimension: BudgetDimension,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        execution.cancellation.cancel();
        execution.pending_budget_exhaustion = None;
        execution.pending_tool_calls.clear();
        execution.subagent_budget_reservations.clear();
        let event = execution
            .machine
            .record_budget_exhausted(dimension)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.terminal_event = Some(event.clone());
        Ok(event)
    }

    pub fn remaining_duration(&self, attempt_id: Uuid) -> Result<Duration, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        Ok(execution
            .execution_time
            .remaining(execution.command.budget.max_duration_seconds))
    }

    /// Stops charging active execution time before an approval checkpoint is
    /// published. A restored attempt starts a fresh monotonic slice.
    pub fn pause_duration_budget(&mut self, attempt_id: Uuid) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.terminal_event.is_none() {
            execution.execution_time.pause();
        }
        Ok(())
    }

    pub fn timeout_duration(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
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
        if tool_outcome_uncertainty(execution)?.is_some() {
            return self.terminate_uncertain_tool_with_interruption(
                attempt_id,
                Some(RunInterruption::DurationTimeout),
            );
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .expect("execution was checked above");
        execution.cancellation.cancel();
        execution.pending_tool_calls.clear();
        execution.subagent_budget_reservations.clear();
        let event = execution
            .machine
            .record_duration_timed_out()
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
        if tool_outcome_uncertainty(execution)?.is_some() {
            return self.terminate_uncertain_tool_with_interruption(
                attempt_id,
                Some(RunInterruption::Cancellation),
            );
        }
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .expect("execution was checked above");
        execution.cancellation.cancel();
        execution.subagent_budget_reservations.clear();
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

    /// Replaces the attempt-local token with a supervisor-owned cancellation
    /// root before execution starts. Embedded hosts use this to make model,
    /// MCP discovery and Tool work share one downward-only cancellation tree.
    pub fn bind_cancellation_token(
        &mut self,
        attempt_id: Uuid,
        cancellation: CancellationToken,
    ) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if execution.terminal_event.is_some() {
            return Err(WorkerAssignmentError::AttemptAlreadyTerminal);
        }
        execution.cancellation = cancellation;
        Ok(())
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
            || !execution.pending_subagents.is_empty()
            || !execution.pending_tool_calls.is_empty()
            || !execution.outstanding_tool_calls.is_empty()
        {
            return Err(WorkerAssignmentError::ToolTurnIncomplete);
        }
        if execution.pending_budget_exhaustion.is_some() {
            return Err(WorkerAssignmentError::BudgetExhausted);
        }
        let command = &execution.command;
        let reserved = subagent_budget_reservation_totals(&execution.subagent_budget_reservations);
        let remaining_tokens = command
            .budget
            .max_tokens
            .saturating_sub(execution.budget_usage.tokens)
            .saturating_sub(reserved.max_tokens);
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
        let runtime_policy_snapshot_json = command
            .runtime_policy
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| WorkerAssignmentError::InvalidCommand(error.to_string()))?
            .unwrap_or_default();
        let runtime_policy_digest = if runtime_policy_snapshot_json.is_empty() {
            String::new()
        } else {
            digest_bytes(&runtime_policy_snapshot_json)
        };
        // Federated definitions sit alongside native ones and go through the
        // same filter. The intersection is unchanged (ADR-0040 decision 5): a
        // federated tool is offered only if the Skill declared it by qualified
        // name and the AgentVersion delegated its server's scope.
        let registry = execution
            .federated_registry
            .as_ref()
            .unwrap_or(&self.tool_registry);
        let mut tools = self
            .tool_definitions
            .values()
            .chain(execution.federated_definitions.iter())
            .filter(|definition| {
                (execution
                    .effective_tool_names
                    .contains(&definition.descriptor.name)
                    || (mcp_gateway::is_runtime_mcp_read_tool(&definition.descriptor.name)
                        && !execution.runtime_mcp_read_servers.is_empty()))
                    && registry
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
            tools.push(subagent_wait_tool());
            tools.push(subagent_close_tool());
            tools.push(subagent_send_tool());
            tools.push(subagent_history_tool());
            tools.push(subagent_fork_tool());
            tools.push(subagent_rollback_tool());
        }
        Ok(PreparedModelInvocation {
            invocation: ModelInvocation {
                schema_version: if command.schema_version >= 20 {
                    5
                } else if !runtime_policy_snapshot_json.is_empty() {
                    4
                } else if !model_policy_snapshot_json.is_empty() {
                    3
                } else {
                    2
                },
                tenant_id: command.tenant_id.to_string(),
                application_id: if command.schema_version >= 20 {
                    command.application_id.to_string()
                } else {
                    String::new()
                },
                workload_identity_id: if command.schema_version >= 20 {
                    command.workload_identity_id.to_string()
                } else {
                    String::new()
                },
                run_id: command.run_id.to_string(),
                session_id: command.session_id.to_string(),
                workspace_id: if command.schema_version >= 20 {
                    command.workspace_id.to_string()
                } else {
                    String::new()
                },
                agent_version_id: if command.schema_version >= 20 {
                    command.agent_version_id.to_string()
                } else {
                    String::new()
                },
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
                runtime_policy_snapshot_json,
                runtime_policy_digest,
            },
            workload_token: command.workload_token.clone(),
        })
    }

    /// Prepares a provider-neutral summarization request for the oldest safe
    /// transcript prefix. The prepared binding is stored before model egress;
    /// retry or recovery therefore rebuilds the same request instead of moving
    /// the cut point under a result that is already in flight.
    pub fn prepare_transcript_compaction(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<Option<PreparedTranscriptCompaction>, WorkerAssignmentError> {
        let mut base = self.prepare_model_invocation(attempt_id)?;
        let pending = {
            let execution = self
                .accepted
                .get(&attempt_id)
                .ok_or(WorkerAssignmentError::UnknownAttempt)?;
            let Some(policy) = execution
                .command
                .runtime_policy
                .as_ref()
                .map(|policy| &policy.context_compaction)
                .filter(|policy| policy.enabled)
            else {
                return Ok(None);
            };
            if !execution.assistant_text_buffer.is_empty()
                || !transcript_has_complete_tool_pairs(&execution.transcript)
            {
                return Err(WorkerAssignmentError::InvalidTranscriptCompaction);
            }
            if let Some(pending) = execution.pending_transcript_compaction.as_ref() {
                if !pending.is_valid_for(&execution.command, &execution.transcript) {
                    return Err(WorkerAssignmentError::InvalidTranscriptCompaction);
                }
                pending.clone()
            } else {
                if execution
                    .transcript_compaction
                    .as_ref()
                    .is_some_and(|record| {
                        usize::try_from(record.compacted_message_count).ok()
                            == Some(execution.transcript.len())
                    })
                {
                    return Ok(None);
                }
                if model_messages_size(&execution.transcript) <= policy.trigger_bytes {
                    return Ok(None);
                }
                let system_count = execution
                    .transcript
                    .iter()
                    .take_while(|message| message.role == ModelRole::System as i32)
                    .count();
                let mut retained_start = execution.transcript.len();
                let mut retained_bytes = 0_u64;
                while retained_start > system_count && retained_bytes < policy.retain_bytes {
                    retained_start -= 1;
                    retained_bytes = retained_bytes.saturating_add(
                        u64::try_from(execution.transcript[retained_start].encoded_len())
                            .unwrap_or(u64::MAX),
                    );
                }
                while retained_start > system_count
                    && (!transcript_has_complete_tool_pairs(
                        &execution.transcript[system_count..retained_start],
                    ) || !transcript_has_complete_tool_pairs(
                        &execution.transcript[retained_start..],
                    ))
                {
                    retained_start -= 1;
                }
                if retained_start <= system_count {
                    return Ok(None);
                }
                let source = &execution.transcript[system_count..retained_start];
                let retained = &execution.transcript[retained_start..];
                let source_transcript_digest = model_messages_digest(&execution.transcript);
                let source_prefix_digest = model_messages_digest(source);
                let retained_tail_digest = model_messages_digest(retained);
                let source_message_count = u32::try_from(source.len())
                    .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
                let retained_message_count = u32::try_from(retained.len())
                    .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
                PendingTranscriptCompaction {
                    binding_digest: compaction_binding_digest(
                        &execution.command,
                        &source_transcript_digest,
                        &source_prefix_digest,
                        source_message_count,
                        &retained_tail_digest,
                        retained_message_count,
                    ),
                    source_transcript_digest,
                    source_prefix_digest,
                    source_message_count,
                    retained_tail_digest,
                    retained_message_count,
                    system_message_count: u32::try_from(system_count)
                        .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?,
                    retained_start: u32::try_from(retained_start)
                        .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?,
                }
            }
        };

        {
            let execution = self
                .accepted
                .get_mut(&attempt_id)
                .ok_or(WorkerAssignmentError::UnknownAttempt)?;
            execution.pending_transcript_compaction = Some(pending.clone());
            let system_count = usize::try_from(pending.system_message_count)
                .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
            let retained_start = usize::try_from(pending.retained_start)
                .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
            let mut messages = Vec::with_capacity(
                usize::try_from(pending.source_message_count)
                    .unwrap_or(usize::MAX)
                    .saturating_add(2),
            );
            messages.push(ModelMessage {
                role: ModelRole::System as i32,
                content: vec![ContentPart {
                    body: Some(content_part::Body::Text(TextPart {
                        text: COMPACTION_SYSTEM_PROMPT.into(),
                    })),
                }],
            });
            messages.extend_from_slice(&execution.transcript[system_count..retained_start]);
            messages.push(ModelMessage {
                role: ModelRole::User as i32,
                content: vec![ContentPart {
                    body: Some(content_part::Body::Text(TextPart {
                        text: COMPACTION_FINAL_INSTRUCTION.into(),
                    })),
                }],
            });
            base.invocation.messages = messages;
            base.invocation.tools.clear();
            base.invocation.output_schema_json.clear();
            let policy = &execution
                .command
                .runtime_policy
                .as_ref()
                .expect("validated compaction has runtime policy")
                .context_compaction;
            base.invocation.max_output_tokens = base
                .invocation
                .max_output_tokens
                .min(policy.max_summary_tokens);
        }

        Ok(Some(PreparedTranscriptCompaction {
            invocation: base.invocation,
            workload_token: base.workload_token,
            binding_digest: pending.binding_digest,
            source_message_count: pending.source_message_count,
            retained_message_count: pending.retained_message_count,
        }))
    }

    pub fn apply_transcript_compaction(
        &mut self,
        attempt_id: Uuid,
        binding_digest: &str,
        summary: &str,
        input_tokens: u64,
        output_tokens: u64,
        cost_micros: u64,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let pending = execution
            .pending_transcript_compaction
            .clone()
            .filter(|pending| pending.binding_digest == binding_digest)
            .ok_or(WorkerAssignmentError::TranscriptCompactionBindingMismatch)?;
        if !pending.is_valid_for(&execution.command, &execution.transcript)
            || summary.trim().is_empty()
            || summary.len() > COMPACTION_SUMMARY_MAX_BYTES
        {
            return Err(WorkerAssignmentError::InvalidTranscriptCompaction);
        }
        execution.budget_usage.tokens = execution
            .budget_usage
            .tokens
            .saturating_add(input_tokens.saturating_add(output_tokens));
        execution.budget_usage.cost_micros = execution
            .budget_usage
            .cost_micros
            .saturating_add(cost_micros);
        if let Some(exhaustion) =
            budget_exhaustion(execution.budget_usage, &execution.command.budget)
        {
            execution.pending_transcript_compaction = None;
            let event = execution
                .machine
                .record_budget_exhausted(exhaustion.dimension)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution.terminal_event = Some(event.clone());
            return Ok(event);
        }

        let system_count = usize::try_from(pending.system_message_count)
            .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
        let retained_start = usize::try_from(pending.retained_start)
            .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
        let summary_message = ModelMessage {
            role: ModelRole::User as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::Text(TextPart {
                    text: format!("{COMPACTION_SUMMARY_PREFIX}\n{}", summary.trim()),
                })),
            }],
        };
        let mut compacted = Vec::with_capacity(
            system_count
                .saturating_add(1)
                .saturating_add(execution.transcript.len().saturating_sub(retained_start)),
        );
        compacted.extend_from_slice(&execution.transcript[..system_count]);
        let summary_message_index = u32::try_from(compacted.len())
            .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?;
        compacted.push(summary_message.clone());
        compacted.extend_from_slice(&execution.transcript[retained_start..]);
        let record = TranscriptCompactionRecord {
            binding_digest: pending.binding_digest.clone(),
            source_transcript_digest: pending.source_transcript_digest,
            source_prefix_digest: pending.source_prefix_digest,
            source_message_count: pending.source_message_count,
            retained_tail_digest: pending.retained_tail_digest,
            retained_message_count: pending.retained_message_count,
            summary_digest: model_messages_digest(&[summary_message]),
            compacted_transcript_digest: model_messages_digest(&compacted),
            compacted_message_count: u32::try_from(compacted.len())
                .map_err(|_| WorkerAssignmentError::InvalidTranscriptCompaction)?,
            summary_message_index,
        };
        execution.transcript = compacted;
        execution.pending_transcript_compaction = None;
        execution.transcript_compaction = Some(record.clone());
        execution
            .machine
            .record_context_compacted(
                &record.binding_digest,
                &record.source_transcript_digest,
                &record.summary_digest,
                &record.retained_tail_digest,
                record.source_message_count,
                record.retained_message_count,
            )
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
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
        if call.name == "agent.history" {
            let arguments: SubagentHistoryArguments =
                serde_json::from_value(call.arguments.clone())
                    .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            let _after_activation_ordinal = arguments.after_activation_ordinal;
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || arguments.limit == 0
                || arguments.limit > 50
                || !execution.subagent_handles.contains_key(&arguments.agent_id)
                || execution
                    .subagent_generations
                    .get(&arguments.agent_id)
                    .is_none_or(|current| {
                        arguments
                            .generation
                            .is_some_and(|generation| generation == 0 || generation > *current)
                    })
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request = subagent_control_request(&execution.command, call, ToolEffect::Pure);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.wait" {
            let arguments: SubagentWaitArguments =
                serde_json::from_value(call.arguments.clone())
                    .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || arguments.timeout_ms == 0
                || arguments.timeout_ms > 300_000
                || (!execution.active_subagents.contains_key(&arguments.agent_id)
                    && !execution
                        .completed_subagents
                        .contains_key(&arguments.agent_id))
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request = subagent_control_request(&execution.command, call, ToolEffect::Pure);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.close" {
            let arguments: SubagentCloseArguments = serde_json::from_value(call.arguments.clone())
                .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || (!execution.active_subagents.contains_key(&arguments.agent_id)
                    && !execution
                        .completed_subagents
                        .contains_key(&arguments.agent_id))
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request =
                subagent_control_request(&execution.command, call, ToolEffect::Idempotent);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.send" {
            let arguments: SubagentSendArguments =
                serde_json::from_value(call.arguments.clone())
                    .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            let message_digest = digest_bytes(arguments.message.as_bytes());
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || arguments.message.trim().is_empty()
                || arguments.message.len() > 32_000
                || !valid_subagent_message_idempotency_key(&arguments.idempotency_key)
                || !execution.subagent_handles.contains_key(&arguments.agent_id)
                || execution
                    .subagent_generations
                    .get(&arguments.agent_id)
                    .is_none_or(|current| {
                        (*current > 1 && arguments.generation != Some(*current))
                            || arguments
                                .generation
                                .is_some_and(|generation| generation != *current)
                    })
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let replay = execution
                .subagent_message_receipts
                .get(&arguments.agent_id)
                .and_then(|receipts| receipts.get(&arguments.idempotency_key));
            if let Some(receipt) = replay {
                if receipt.message_digest != message_digest {
                    return Err(WorkerAssignmentError::SubagentMessageConflict);
                }
            } else if (!execution
                .completed_subagents
                .contains_key(&arguments.agent_id)
                && !execution.active_subagents.contains_key(&arguments.agent_id))
                || execution.closed_subagents.contains(&arguments.agent_id)
                || execution
                    .subagent_message_queues
                    .get(&arguments.agent_id)
                    .is_some_and(|queue| queue.len() >= 8)
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request =
                subagent_control_request(&execution.command, call, ToolEffect::Idempotent);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.fork" {
            let arguments: SubagentForkArguments =
                serde_json::from_value(call.arguments.clone())
                    .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            let source = execution
                .subagent_handles
                .get(&arguments.source_agent_id)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            let history = execution
                .subagent_conversations
                .get(&arguments.source_agent_id)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || execution
                    .subagent_generations
                    .get(&arguments.source_agent_id)
                    != Some(&arguments.source_generation)
                || !history
                    .iter()
                    .any(|turn| turn.activation_ordinal == arguments.through_activation_ordinal)
                || arguments.max_tokens == 0
                || arguments.max_tokens > source.budget.max_tokens
                || arguments.max_cost_cents == 0
                || arguments.max_cost_cents > source.budget.max_cost_cents
                || arguments.max_duration_seconds == 0
                || arguments.max_duration_seconds > source.budget.max_duration_seconds
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request =
                subagent_control_request(&execution.command, call, ToolEffect::Idempotent);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.rollback" {
            let arguments: SubagentRollbackArguments =
                serde_json::from_value(call.arguments.clone())
                    .map_err(|_| WorkerAssignmentError::InvalidToolCall)?;
            let history = execution
                .subagent_conversations
                .get(&arguments.agent_id)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            let boundary_index = history
                .iter()
                .position(|turn| turn.activation_ordinal == arguments.through_activation_ordinal)
                .ok_or(WorkerAssignmentError::InvalidToolCall)?;
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || execution.subagent_generations.get(&arguments.agent_id)
                    != Some(&arguments.generation)
                || boundary_index + 1 >= history.len()
                || execution.active_subagents.contains_key(&arguments.agent_id)
                || execution.closed_subagents.contains(&arguments.agent_id)
                || !execution
                    .completed_subagents
                    .contains_key(&arguments.agent_id)
                || execution
                    .subagent_message_queues
                    .get(&arguments.agent_id)
                    .is_some_and(|queue| !queue.is_empty())
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let request =
                subagent_control_request(&execution.command, call, ToolEffect::Idempotent);
            let plan = ToolPlan::Execute(request.clone());
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .apply_tool_plan(&plan)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution
                .outstanding_tool_calls
                .insert(request.call.id.clone(), request);
            return Ok(PlannedToolCall {
                plan,
                event,
                followup_event: None,
                subagent_request: None,
            });
        }
        if call.name == "agent.spawn" {
            if execution.pending_subagents.len() + execution.active_subagents.len() >= 8 {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
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
            let reserved =
                subagent_budget_reservation_totals(&execution.subagent_budget_reservations);
            if !execution.command.delegated_scopes.contains("agent:spawn")
                || role
                    .delegated_scopes
                    .iter()
                    .any(|scope| !execution.command.delegated_scopes.contains(scope))
                || arguments.max_tokens > remaining_tokens.saturating_sub(reserved.max_tokens)
                || arguments.max_cost_cents
                    > remaining_cost_cents.saturating_sub(reserved.max_cost_cents)
            {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let remaining_duration = execution
                .execution_time
                .remaining(execution.command.budget.max_duration_seconds);
            if remaining_duration.is_zero() {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            let remaining_duration_seconds =
                u64::try_from(remaining_duration.as_millis().saturating_add(999) / 1_000)
                    .unwrap_or(u64::MAX)
                    .saturating_sub(reserved.max_duration_seconds);
            let mut arguments = arguments;
            arguments.max_duration_seconds = arguments
                .max_duration_seconds
                .min(remaining_duration_seconds);
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
                mode: arguments.mode,
                conversation_history: Vec::new(),
            };
            if !request.is_well_formed() {
                return Err(WorkerAssignmentError::InvalidToolCall);
            }
            execution.pending_tool_calls.pop_front();
            let event = execution
                .machine
                .record_subagent_spawn_requested(&request)
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            insert_subagent_budget_reservation(
                &mut execution.subagent_budget_reservations,
                request.delegation_id,
                &request,
            )?;
            execution.pending_subagents.push(request.clone());
            return Ok(PlannedToolCall {
                plan: ToolPlan::SubagentSpawn(request.clone()),
                event,
                followup_event: None,
                subagent_request: Some(request),
            });
        }
        let runtime_mcp_read = mcp_gateway::is_runtime_mcp_read_tool(&call.name);
        if runtime_mcp_read
            && !mcp_gateway::runtime_mcp_read_call_is_authorized(
                &call.name,
                &call.arguments,
                &execution.runtime_mcp_read_servers,
                &execution.command.delegated_scopes,
            )
        {
            return Err(WorkerAssignmentError::InvalidToolCall);
        }
        if !runtime_mcp_read && !execution.effective_tool_names.contains(&call.name) {
            return Err(WorkerAssignmentError::ToolConfiguration(format!(
                "tool {} is not activated by the execution Skill snapshot",
                call.name
            )));
        }
        // The Run's own registry when it has one -- the Worker's native Tools
        // plus this Run's federated ones. Planning against the Worker's would
        // leave every federated call an unknown tool.
        let registry = execution
            .federated_registry
            .as_ref()
            .unwrap_or(&self.tool_registry);
        let plan = registry
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
                execution.execution_time.pause();
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
        execution.execution_time.resume();
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
        execution.execution_time.resume();

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
        execution.applied_approval_decisions.insert(
            command.approval_id,
            AppliedApprovalDecision {
                binding_digest: command.binding_digest.clone(),
                decision: command.decision,
            },
        );
        execution.approval_decisions.insert(
            command.approval_id,
            ApprovalDecisionReceipt {
                command,
                outcome: outcome.clone(),
            },
        );
        Ok(outcome)
    }

    /// Reports whether a restored Checkpoint already incorporated this exact
    /// decision. A different digest or decision is not idempotent.
    pub fn approval_decision_was_checkpointed(
        &self,
        attempt_id: Uuid,
        approval_id: Uuid,
        binding_digest: &str,
        decision: ToolApprovalDecision,
    ) -> Result<bool, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        Ok(execution
            .applied_approval_decisions
            .get(&approval_id)
            .is_some_and(|applied| {
                applied.binding_digest == binding_digest && applied.decision == decision
            }))
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

    /// Freezes one replay-safe Tool batch in the assistant's source order.
    /// Planning and approval remain sequential; only execution may overlap.
    pub fn begin_ordered_tool_batch(
        &mut self,
        attempt_id: Uuid,
        requests: &[ToolExecutionRequest],
    ) -> Result<(), WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let max_concurrent_tools = execution
            .command
            .runtime_policy
            .as_ref()
            .map_or(1, |policy| policy.tool_execution.max_concurrent_tools);
        let ids = requests
            .iter()
            .map(|request| request.call.id.as_str())
            .collect::<BTreeSet<_>>();
        let requested_order = requests
            .iter()
            .map(|request| request.call.id.as_str())
            .collect::<Vec<_>>();
        let assistant_order = execution
            .transcript
            .iter()
            .rev()
            .find(|message| message.role == ModelRole::Assistant as i32)
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|part| match part.body.as_ref() {
                        Some(content_part::Body::ToolCall(call))
                            if ids.contains(call.tool_call_id.as_str()) =>
                        {
                            Some(call.tool_call_id.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if requests.len() < 2
            || requests.len() > usize::from(max_concurrent_tools)
            || ids.len() != requests.len()
            || assistant_order != requested_order
            || execution.pending_approval.is_some()
            || !execution.ordered_tool_commit_queue.is_empty()
            || !execution.staged_ordered_tool_results.is_empty()
            || execution.outstanding_tool_calls.len() != requests.len()
            || requests.iter().any(|request| {
                request.effect != ToolEffect::Pure
                    || execution
                        .outstanding_tool_calls
                        .get(&request.call.id)
                        .is_none_or(|planned| planned != request)
                    || execution.started_tool_calls.contains_key(&request.call.id)
            })
        {
            return Err(WorkerAssignmentError::InvalidParallelToolBatch);
        }
        execution.ordered_tool_commit_queue = requests
            .iter()
            .map(|request| request.call.id.clone())
            .collect();
        Ok(())
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
        if let Some((digest, event)) = execution.subagent_result_receipts.get(&result.tool_call_id)
        {
            return if digest == &result.digest {
                Ok(event.clone())
            } else {
                Err(WorkerAssignmentError::SubagentResultBindingMismatch)
            };
        }
        let request_index = execution
            .pending_subagents
            .iter()
            .position(|request| {
                request.tool_call_id == result.tool_call_id
                    && request.delegation_id == result.delegation_id
                    && request.binding_digest == result.binding_digest
            })
            .ok_or(WorkerAssignmentError::SubagentResultBindingMismatch)?;
        let request = execution.pending_subagents[request_index].clone();
        if !result.is_well_formed() {
            return Err(WorkerAssignmentError::SubagentResultBindingMismatch);
        }
        let tool_call_id = request.tool_call_id.clone();
        let remaining_subagents = execution.pending_subagents.len().saturating_sub(1);
        let event = execution
            .machine
            .record_subagent_result_received_with_remaining(result, remaining_subagents)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        remove_subagent_budget_reservation(
            &mut execution.subagent_budget_reservations,
            request.delegation_id,
            &request,
        )?;
        execution.pending_subagents.remove(request_index);
        execution.budget_usage.tokens = execution
            .budget_usage
            .tokens
            .saturating_add(result.usage.tokens);
        execution.budget_usage.cost_micros = execution
            .budget_usage
            .cost_micros
            .saturating_add(result.usage.cost_micros);
        execution.pending_budget_exhaustion =
            budget_exhaustion(execution.budget_usage, &execution.command.budget);
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
        execution.subagent_result_receipts.insert(
            result.tool_call_id.clone(),
            (result.digest.clone(), event.clone()),
        );
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

    pub fn record_tool_execution_progress(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
        binding_digest: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
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
        if !execution.started_tool_calls.contains_key(tool_call_id) {
            return Err(WorkerAssignmentError::ToolExecutionNotStarted);
        }
        execution
            .machine
            .record_tool_execution_progress(
                &request.call.id,
                &request.binding_digest,
                progress,
                total,
                message,
            )
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_mcp_input_required(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: &str,
        binding_digest: &str,
        server_id: Uuid,
        server_name: &str,
        round: u8,
        request_state: String,
        requests: BTreeMap<String, McpElicitationRequest>,
    ) -> Result<McpInputRequiredReceipt, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let request = execution
            .outstanding_tool_calls
            .get(tool_call_id)
            .filter(|request| request.binding_digest == binding_digest)
            .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
        if !execution.started_tool_calls.contains_key(tool_call_id)
            || execution.pending_mcp_input.is_some()
        {
            return Err(WorkerAssignmentError::McpInputBindingMismatch);
        }
        let server = execution
            .command
            .mcp_servers
            .iter()
            .find(|server| server.server_id == server_id && server.name == server_name)
            .filter(|server| {
                server.protocol_revision == McpProtocolRevision::V2026_07_28
                    && server
                        .client_capabilities
                        .contains(&McpClientCapability::Elicitation)
            })
            .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
        let qualified_prefix = format!("mcp:{}/", server.name);
        if !request.call.name.starts_with(&qualified_prefix) {
            return Err(WorkerAssignmentError::McpInputBindingMismatch);
        }
        match execution.resolved_mcp_input.as_ref() {
            Some(resolved)
                if resolved.pending.tool_call_id == tool_call_id
                    && resolved.pending.binding_digest == binding_digest
                    && resolved.continuation_started.is_some()
                    && resolved.continuation.round == round => {}
            None if round == 1 => {}
            _ => return Err(WorkerAssignmentError::McpInputBindingMismatch),
        }
        // A round-ten response is terminal by contract; accepting another
        // user-input request here would create a continuation that no adapter
        // is permitted to dispatch.
        if round >= 10 {
            return Err(WorkerAssignmentError::InvalidMcpInputResolution(
                "MCP MRTR input request exceeds the ten-round limit".into(),
            ));
        }
        let pending = McpInputRequired {
            schema_version: agent_protocol::MCP_INPUT_REQUIRED_SCHEMA_VERSION,
            input_id: Uuid::now_v7(),
            server_id,
            server_name: server_name.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            binding_digest: binding_digest.to_owned(),
            round,
            request_state,
            requests,
        };
        pending
            .validate()
            .map_err(|error| WorkerAssignmentError::InvalidMcpInputResolution(error.to_string()))?;
        let event = execution
            .machine
            .record_mcp_input_required(&pending)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.execution_time.pause();
        execution.resolved_mcp_input = None;
        execution.pending_mcp_input = Some(pending.clone());
        Ok(McpInputRequiredReceipt { event, pending })
    }

    pub fn apply_mcp_input_resolution(
        &mut self,
        command: McpInputResolutionCommand,
        received_at: DateTime<Utc>,
    ) -> Result<McpInputResolutionReceipt, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&command.attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let active = &execution.command;
        if command.tenant_id != active.tenant_id
            || command.run_id != active.run_id
            || command.attempt_id != active.attempt_id
            || command.worker_id != active.worker_id
            || command.worker_incarnation_id != active.worker_incarnation_id
            || command.issued_at > received_at
            || received_at >= command.expires_at
        {
            return Err(WorkerAssignmentError::McpInputBindingMismatch);
        }
        if let Some(resolved) = execution.resolved_mcp_input.as_ref() {
            command.validate_for(&resolved.pending).map_err(|error| {
                WorkerAssignmentError::InvalidMcpInputResolution(error.to_string())
            })?;
            if command.responses != resolved.continuation.responses {
                return Err(WorkerAssignmentError::McpInputBindingMismatch);
            }
            let request = execution
                .outstanding_tool_calls
                .get(&resolved.pending.tool_call_id)
                .cloned()
                .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
            return Ok(McpInputResolutionReceipt {
                event: resolved.resolution_event.clone(),
                request,
                continuation: resolved.continuation.clone(),
            });
        }
        let pending = execution
            .pending_mcp_input
            .as_ref()
            .cloned()
            .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
        command
            .validate_for(&pending)
            .map_err(|error| WorkerAssignmentError::InvalidMcpInputResolution(error.to_string()))?;
        let request = execution
            .outstanding_tool_calls
            .get(&pending.tool_call_id)
            .filter(|request| request.binding_digest == pending.binding_digest)
            .cloned()
            .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
        let continuation = McpInputContinuation {
            round: pending.round.saturating_add(1),
            request_state: pending.request_state.clone(),
            responses: command.responses,
        };
        if !(2..=10).contains(&continuation.round) {
            return Err(WorkerAssignmentError::InvalidMcpInputResolution(
                "MCP MRTR continuation exceeds the ten-round limit".into(),
            ));
        }
        let event = execution
            .machine
            .record_mcp_input_resolved(&pending, &continuation)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        execution.execution_time.resume();
        execution.pending_mcp_input = None;
        execution.resolved_mcp_input = Some(ResolvedMcpInput {
            pending,
            continuation: continuation.clone(),
            resolution_event: event.clone(),
            continuation_started: None,
        });
        Ok(McpInputResolutionReceipt {
            event,
            request,
            continuation,
        })
    }

    pub fn record_mcp_continuation_started(
        &mut self,
        attempt_id: Uuid,
        input_id: Uuid,
    ) -> Result<EventEnvelope, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        let resolved = execution
            .resolved_mcp_input
            .as_mut()
            .filter(|resolved| resolved.pending.input_id == input_id)
            .ok_or(WorkerAssignmentError::McpInputBindingMismatch)?;
        if let Some(event) = resolved.continuation_started.as_ref() {
            return Ok(event.clone());
        }
        let event = execution
            .machine
            .record_mcp_continuation_started(&resolved.pending, &resolved.continuation)
            .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
        resolved.continuation_started = Some(event.clone());
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
        if execution
            .pending_mcp_input
            .as_ref()
            .is_some_and(|pending| pending.tool_call_id == tool_call_id)
        {
            return Err(WorkerAssignmentError::McpInputBindingMismatch);
        }
        if execution
            .resolved_mcp_input
            .as_ref()
            .is_some_and(|resolved| resolved.pending.tool_call_id == tool_call_id)
        {
            execution.resolved_mcp_input = None;
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

    /// Stages a completed Tool result and commits every newly contiguous
    /// source-order prefix. The caller must publish every returned event in
    /// order before advancing the Agent Loop.
    pub fn record_bound_tool_result_ordered(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: &str,
        content: serde_json::Value,
        is_error: bool,
    ) -> Result<Vec<EventEnvelope>, WorkerAssignmentError> {
        let execution = self
            .accepted
            .get_mut(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?;
        if !execution
            .ordered_tool_commit_queue
            .iter()
            .any(|queued| queued == &tool_call_id)
            || execution
                .staged_ordered_tool_results
                .contains_key(&tool_call_id)
        {
            return Err(WorkerAssignmentError::ToolCallNotExecuting);
        }
        let request = execution
            .outstanding_tool_calls
            .get(&tool_call_id)
            .filter(|request| request.binding_digest == binding_digest)
            .cloned()
            .ok_or(WorkerAssignmentError::ToolResultBindingMismatch)?;
        if request.effect != ToolEffect::Pure {
            return Err(WorkerAssignmentError::InvalidParallelToolBatch);
        }
        if !execution.started_tool_calls.contains_key(&tool_call_id) {
            return Err(WorkerAssignmentError::ToolExecutionNotStarted);
        }
        execution.outstanding_tool_calls.remove(&tool_call_id);
        execution.started_tool_calls.remove(&tool_call_id);
        execution.staged_ordered_tool_results.insert(
            tool_call_id,
            StagedOrderedToolResult {
                request,
                content,
                is_error,
            },
        );

        let mut committed = Vec::new();
        while let Some(next_id) = execution.ordered_tool_commit_queue.front().cloned() {
            let Some(staged) = execution.staged_ordered_tool_results.remove(&next_id) else {
                break;
            };
            execution.ordered_tool_commit_queue.pop_front();
            let event = execution
                .machine
                .record_tool_result(
                    &next_id,
                    &staged.request.binding_digest,
                    staged.content.clone(),
                    staged.is_error,
                )
                .map_err(|error| WorkerAssignmentError::KernelTransition(error.to_string()))?;
            execution.transcript.push(ModelMessage {
                role: ModelRole::Tool as i32,
                content: vec![ContentPart {
                    body: Some(content_part::Body::ToolResult(ToolResultPart {
                        tool_call_id: next_id,
                        content_json: serde_json::to_vec(&staged.content)
                            .expect("tool result content is serializable"),
                    })),
                }],
            });
            committed.push(event);
        }
        Ok(committed)
    }

    /// Records one completed Tool using the commit semantics frozen for its
    /// active batch. Serial calls yield exactly one event; an ordered batch
    /// may yield none until an earlier source-order result arrives, or several
    /// once a contiguous prefix becomes available.
    pub fn record_bound_tool_completion(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: &str,
        content: serde_json::Value,
        is_error: bool,
    ) -> Result<Vec<EventEnvelope>, WorkerAssignmentError> {
        let ordered = self
            .accepted
            .get(&attempt_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)?
            .ordered_tool_commit_queue
            .iter()
            .any(|queued| queued == &tool_call_id);
        if ordered {
            self.record_bound_tool_result_ordered(
                attempt_id,
                tool_call_id,
                binding_digest,
                content,
                is_error,
            )
        } else {
            self.record_bound_tool_result(
                attempt_id,
                tool_call_id,
                binding_digest,
                content,
                is_error,
            )
            .map(|event| vec![event])
        }
    }

    /// Classifies an executor failure against the frozen Tool effect before
    /// changing durable Run state. A proven pre-side-effect failure, or a Tool
    /// whose contract is replay-safe, can become a model-visible error result.
    /// Every other started NonIdempotent/Unknown failure is an ambiguous
    /// external side effect and must terminate through the existing bound
    /// `run.indeterminate` reconciliation path.
    pub fn record_tool_execution_failure(
        &mut self,
        attempt_id: Uuid,
        tool_call_id: String,
        binding_digest: &str,
        error: &ToolExecutionError,
    ) -> Result<Vec<EventEnvelope>, WorkerAssignmentError> {
        let effect = {
            let execution = self
                .accepted
                .get(&attempt_id)
                .ok_or(WorkerAssignmentError::UnknownAttempt)?;
            let request = execution
                .outstanding_tool_calls
                .get(&tool_call_id)
                .ok_or(WorkerAssignmentError::ToolCallNotExecuting)?;
            if request.binding_digest != binding_digest {
                return Err(WorkerAssignmentError::ToolResultBindingMismatch);
            }
            if !execution.started_tool_calls.contains_key(&tool_call_id) {
                return Err(WorkerAssignmentError::ToolExecutionNotStarted);
            }
            request.effect
        };
        if error.deterministic_failure_result().is_some()
            || matches!(effect, ToolEffect::Pure | ToolEffect::Idempotent)
        {
            return self.record_bound_tool_completion(
                attempt_id,
                tool_call_id,
                binding_digest,
                tool_execution_failure_content(error),
                true,
            );
        }
        self.terminate_uncertain_tool(attempt_id)
            .map(|event| vec![event])
    }

    pub fn ordered_tool_batch_active(
        &self,
        attempt_id: Uuid,
    ) -> Result<bool, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| !execution.ordered_tool_commit_queue.is_empty())
            .ok_or(WorkerAssignmentError::UnknownAttempt)
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
        let configured_timeout = execution
            .command
            .runtime_policy
            .as_ref()
            .map(|policy| Duration::from_millis(policy.tool_execution.timeout_ms))
            .unwrap_or_else(|| Duration::from_secs(300));
        let timeout = remaining.min(configured_timeout);
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
            application_id: execution.command.application_id,
            workload_identity_id: execution.command.workload_identity_id,
            run_id: execution.command.run_id,
            session_id: execution.command.session_id,
            workspace_id: execution.command.workspace_id,
            agent_version_id: execution.command.agent_version_id,
            attempt_id,
            workspace_root,
            timeout,
            cancellation: execution.cancellation.clone(),
            requested_at,
        })
    }

    pub fn execution_session_id(&self, attempt_id: Uuid) -> Result<Uuid, WorkerAssignmentError> {
        self.accepted
            .get(&attempt_id)
            .map(|execution| execution.command.session_id)
            .ok_or(WorkerAssignmentError::UnknownAttempt)
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
            || !execution.pending_subagents.is_empty()
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

    /// Rolls back a recovery admission that failed before its restored event was
    /// published. This is intentionally not a general cancellation path.
    fn discard_restored_attempt(&mut self, attempt_id: Uuid) {
        if self
            .accepted
            .get(&attempt_id)
            .is_some_and(|execution| execution.restored_from_checkpoint.is_some())
        {
            self.accepted.remove(&attempt_id);
        }
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
    TimedOut,
    Steered,
    ApprovalApplied,
    ModelEventPublished,
    ModelExecutionFinished,
    ToolExecutionRequested,
    ToolExecutionStarted,
    ToolResultStaged,
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
    pending_tool_events: VecDeque<EventEnvelope>,
    /// Present only when the deployment configured a gateway for federation. A
    /// Run carrying MCP servers on a Worker without one is logged, not failed:
    /// the Run runs without those Tools rather than not at all.
    mcp_federation: Option<GrpcMcpFederationClient>,
}

/// Newtype so `ActiveExecution` keeps its derived `Debug`.
#[derive(Default)]
struct FederatedExecutors(HashMap<String, Arc<dyn ToolExecutor>>);

impl std::fmt::Debug for FederatedExecutors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_set().entries(self.0.keys()).finish()
    }
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
            pending_tool_events: VecDeque::new(),
            mcp_federation: None,
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

    /// Publishes duration terminals for every active attempt whose persisted
    /// execution clock is exhausted. The processor cancels the shared model,
    /// Tool and MCP token before the terminal is published.
    pub async fn poll_duration_once(&mut self) -> Result<WorkerPollResult, WorkerTransportError> {
        let expired = self.processor.expired_duration_attempt_ids();
        if expired.is_empty() {
            return Ok(WorkerPollResult::Idle);
        }
        for attempt_id in expired {
            self.discard_pending_attempt_updates(attempt_id);
            let terminal = self
                .processor
                .timeout_duration(attempt_id)
                .map_err(transport_error)?;
            self.publish_run_event_and_checkpoint(&terminal).await?;
            self.processor
                .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                .map_err(transport_error)?;
        }
        Ok(WorkerPollResult::TimedOut)
    }

    fn discard_pending_attempt_updates(&mut self, attempt_id: Uuid) {
        if self
            .pending_model_event
            .as_ref()
            .is_some_and(|pending| pending.event.attempt_id == attempt_id)
        {
            self.pending_model_event = None;
        }
        if self
            .pending_tool_plan
            .as_ref()
            .is_some_and(|pending| pending.event.attempt_id == attempt_id)
        {
            self.pending_tool_plan = None;
        }
        if self
            .pending_tool_start
            .as_ref()
            .is_some_and(|pending| pending.event.attempt_id == attempt_id)
        {
            self.pending_tool_start = None;
        }
        self.pending_tool_events
            .retain(|event| event.attempt_id != attempt_id);
        self.pending_model_relaunch.remove(&attempt_id);
        self.pending_auth_recovery.remove(&attempt_id);
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

        if command.schema_version >= 20 {
            let Some(verifier) = self.workload_token_verifier.as_ref() else {
                message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        Duration::from_secs(1),
                    )))
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::RetryScheduled);
            };
            if let Err(error) =
                WorkerProcessor::verify_execution_workload_identity(&command, verifier, Utc::now())
            {
                tracing::warn!(%error, "terminating execution command with invalid workload identity");
                message
                    .ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .map_err(transport_error)?;
                return Ok(WorkerPollResult::Terminated);
            }
        }

        // Kept for discovery, which needs the servers and the workload token
        // the command carries and which cannot happen inside accept().
        let federation_source = command.clone();
        match self.processor.accept(command, Utc::now()) {
            Ok(accepted) => {
                self.publish_event(EXECUTION_ACCEPTED_SUBJECT, accepted.message_id, &accepted)
                    .await?;
                let discovery_budget = self
                    .processor
                    .remaining_duration(accepted.attempt_id)
                    .map_err(transport_error)?;
                if discovery_budget.is_zero() {
                    let terminal = self
                        .processor
                        .timeout_duration(accepted.attempt_id)
                        .map_err(transport_error)?;
                    self.publish_run_event_and_checkpoint(&terminal).await?;
                    self.processor
                        .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                        .map_err(transport_error)?;
                    message.double_ack().await.map_err(transport_error)?;
                    return Ok(WorkerPollResult::TimedOut);
                }
                let discovery = tokio::time::timeout(
                    discovery_budget,
                    self.attach_federated_tools(&federation_source, accepted.attempt_id),
                )
                .await;
                match discovery {
                    Ok(result) => result?,
                    Err(_) => {
                        let terminal = self
                            .processor
                            .timeout_duration(accepted.attempt_id)
                            .map_err(transport_error)?;
                        self.publish_run_event_and_checkpoint(&terminal).await?;
                        self.processor
                            .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                            .map_err(transport_error)?;
                        message.double_ack().await.map_err(transport_error)?;
                        return Ok(WorkerPollResult::TimedOut);
                    }
                }
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
            application_id: command.execution.application_id,
            workload_identity_id: command.execution.workload_identity_id,
            run_id: command.execution.run_id,
            session_id: command.execution.session_id,
            workspace_id: command.execution.workspace_id,
            agent_version_id: command.execution.agent_version_id,
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
        let federation_source = command.execution.clone();
        match self
            .processor
            .restore(command.execution, snapshot, Utc::now())
        {
            Ok(restored) => {
                let discovery_budget = self
                    .processor
                    .remaining_duration(restored.accepted.attempt_id)
                    .map_err(transport_error)?;
                if discovery_budget.is_zero() {
                    self.publish_event(
                        EXECUTION_ACCEPTED_SUBJECT,
                        restored.accepted.message_id,
                        &restored.accepted,
                    )
                    .await?;
                    self.publish_run_event_and_checkpoint(&restored.event)
                        .await?;
                    let terminal = self
                        .processor
                        .timeout_duration(restored.accepted.attempt_id)
                        .map_err(transport_error)?;
                    self.publish_run_event_and_checkpoint(&terminal).await?;
                    self.processor
                        .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                        .map_err(transport_error)?;
                    message.double_ack().await.map_err(transport_error)?;
                    return Ok(WorkerPollResult::TimedOut);
                }
                let discovery = tokio::time::timeout(
                    discovery_budget,
                    self.attach_federated_tools(&federation_source, restored.accepted.attempt_id),
                )
                .await;
                let federation_result = match discovery {
                    Ok(result) => result.and_then(|()| {
                        self.processor
                            .verify_restored_federated_tools(restored.accepted.attempt_id)
                            .map_err(transport_error)
                    }),
                    Err(_) => {
                        self.publish_event(
                            EXECUTION_ACCEPTED_SUBJECT,
                            restored.accepted.message_id,
                            &restored.accepted,
                        )
                        .await?;
                        self.publish_run_event_and_checkpoint(&restored.event)
                            .await?;
                        let terminal = self
                            .processor
                            .timeout_duration(restored.accepted.attempt_id)
                            .map_err(transport_error)?;
                        self.publish_run_event_and_checkpoint(&terminal).await?;
                        self.processor
                            .acknowledge_terminal(terminal.attempt_id, terminal.event_id)
                            .map_err(transport_error)?;
                        message.double_ack().await.map_err(transport_error)?;
                        return Ok(WorkerPollResult::TimedOut);
                    }
                };
                if let Err(error) = federation_result {
                    tracing::warn!(%error, "terminating recovery whose frozen MCP catalog cannot be restored");
                    self.processor
                        .discard_restored_attempt(restored.accepted.attempt_id);
                    message
                        .ack_with(async_nats::jetstream::AckKind::Term)
                        .await
                        .map_err(transport_error)?;
                    return Ok(WorkerPollResult::Terminated);
                }
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
                    WorkerRecoveryAction::WaitForMcpInput(_) => WorkerPollResult::Restored,
                    WorkerRecoveryAction::ResumeMcpTool { .. } => {
                        return Err(WorkerTransportError(
                            "recovered MCP continuation requires an MRTR-capable transport path"
                                .into(),
                        ));
                    }
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
                    WorkerRecoveryAction::TerminateIndeterminate(_) => {
                        let terminal = self
                            .processor
                            .terminate_uncertain_tool(restored.accepted.attempt_id)
                            .map_err(transport_error)?;
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
                    WorkerRecoveryAction::RetryToolBatch(requests) => {
                        let attempt_id = restored.accepted.attempt_id;
                        let mut launches = Vec::with_capacity(requests.len());
                        for request in requests {
                            let launch = match self.prepare_tool_launch(attempt_id, request) {
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
                            launches.push(launch);
                        }
                        for (executor, request, context) in launches {
                            let replanned = self
                                .processor
                                .replan_recovered_tool(attempt_id, &request.call.id)
                                .map_err(transport_error)?;
                            self.publish_run_event_and_checkpoint(&replanned).await?;
                            let started = self
                                .processor
                                .record_tool_execution_started(
                                    attempt_id,
                                    &request.call.id,
                                    &request.binding_digest,
                                )
                                .map_err(transport_error)?;
                            self.publish_run_event_and_checkpoint(&started).await?;
                            self.tool_supervisor.start(executor, request, context);
                        }
                        WorkerPollResult::Restored
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
        if !self.pending_tool_events.is_empty() {
            return self.publish_pending_tool_event().await;
        }
        let Some(update) = self.tool_supervisor.recv(timeout).await else {
            return Ok(WorkerPollResult::Idle);
        };
        let (attempt_id, events) = match update {
            ToolExecutionUpdate::Finished {
                attempt_id,
                tool_call_id,
                binding_digest,
                result,
            } => (
                attempt_id,
                self.processor.record_bound_tool_completion(
                    attempt_id,
                    tool_call_id,
                    &binding_digest,
                    result.content,
                    result.is_error,
                ),
            ),
            ToolExecutionUpdate::Failed {
                attempt_id,
                tool_call_id,
                binding_digest,
                error,
            } => (
                attempt_id,
                self.processor.record_tool_execution_failure(
                    attempt_id,
                    tool_call_id,
                    &binding_digest,
                    &error,
                ),
            ),
        };
        let events = events.map_err(transport_error)?;
        if events.is_empty() {
            self.publish_checkpoint(attempt_id).await?;
            return Ok(WorkerPollResult::ToolResultStaged);
        }
        self.pending_tool_events.extend(events);
        self.publish_pending_tool_event().await
    }

    async fn publish_pending_tool_event(
        &mut self,
    ) -> Result<WorkerPollResult, WorkerTransportError> {
        let event = self
            .pending_tool_events
            .front()
            .expect("pending tool event checked by caller");
        let attempt_id = event.attempt_id;
        let event_id = event.event_id;
        // The queue can still hold an earlier Tool result if another poll path
        // terminates the Run. Classify the queued event itself so that stale
        // non-terminal output can never acknowledge an unrelated terminal.
        let terminal = event.event_type == "run.indeterminate";
        self.publish_run_event_and_checkpoint(event).await?;
        self.pending_tool_events.pop_front();
        if terminal {
            self.processor
                .acknowledge_terminal(attempt_id, event_id)
                .map_err(transport_error)?;
            return Ok(WorkerPollResult::Terminated);
        }
        if !self.pending_tool_events.is_empty()
            || self
                .processor
                .ordered_tool_batch_active(attempt_id)
                .map_err(transport_error)?
        {
            return Ok(WorkerPollResult::ToolResultPublished);
        }
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

    /// Discovers this Run's MCP servers and attaches their Tools.
    ///
    /// Once, at the start, before the model is asked anything -- so the catalog
    /// the model is offered is the catalog the Run froze, and a server that
    /// changes later cannot change what this Run may do.
    ///
    /// A server that cannot be reached is logged and skipped rather than failing
    /// the Run. One unreachable third-party server should not stop work that may
    /// never touch it; the Run simply is not offered its Tools, and a Skill
    /// naming one gets an unknown tool rather than a surprise.
    /// Configures federation for this Worker.
    ///
    /// Optional: a deployment without a gateway configured for MCP never offers
    /// federated Tools, and a Run carrying servers says so in the log rather
    /// than failing.
    pub fn set_mcp_federation(&mut self, client: GrpcMcpFederationClient) {
        self.mcp_federation = Some(client);
    }

    async fn attach_federated_tools(
        &mut self,
        command: &RunExecutionCommand,
        attempt_id: Uuid,
    ) -> Result<(), WorkerTransportError> {
        if command.mcp_servers.is_empty() {
            return Ok(());
        }
        let Some(client) = self.mcp_federation.as_ref() else {
            tracing::warn!(
                run_id = %command.run_id,
                servers = command.mcp_servers.len(),
                "run carries mcp servers but this worker has no federation client configured"
            );
            return Ok(());
        };
        let mut client = client.clone();
        let discovered = mcp_gateway::discover_federated_tools(
            self.processor.tool_registry(),
            &mut client,
            command,
            command.workload_token.as_str(),
        )
        .await;
        for (server, reason) in &discovered.unavailable {
            tracing::warn!(
                run_id = %command.run_id, %server, %reason,
                "mcp server was not discoverable; its tools are not offered to this run"
            );
        }
        mcp_gateway::attach_discovered_federated_tools(
            &mut self.processor,
            client,
            command,
            attempt_id,
            discovered,
        )
        .map_err(transport_error)?;
        Ok(())
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
            self.pending_tool_events.push_back(followup_event);
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
        // A federated Tool's executor is per attempt, because the frozen
        // catalog digest it carries is. Checked first so a Worker-wide name
        // could never shadow this Run's own.
        if let Some(executor) = self
            .processor
            .federated_executor(attempt_id, &request.call.name)
        {
            if request.sandbox != SandboxClass::Federated {
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
            return Ok((executor, request, context));
        }
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
        ToolExecutionError::ProcessSessionStartFailed { .. } => "process_session_start_failed",
        ToolExecutionError::McpInputRequired { .. } => "mcp_input_required",
        // Kept distinct from tool_execution_failed on purpose: this is a
        // containment posture failure, not a Tool that misbehaved. An operator
        // seeing it needs to know the sandbox could not be established.
        ToolExecutionError::ContainmentUnavailable(_) => "tool_containment_unavailable",
        // Also a containment posture failure, but a permanent one: this host
        // has no backend that can enforce the boundary, so retrying or moving
        // the Run to another attempt on the same host cannot help. Kept
        // separate from `tool_containment_unavailable`, which means the
        // backend exists and could not be established this time (ADR-0122).
        ToolExecutionError::UnsupportedContainment(_) => "tool_containment_unsupported",
        ToolExecutionError::InvalidDefinition(_)
        | ToolExecutionError::InvalidContext(_)
        | ToolExecutionError::Engine(_)
        | ToolExecutionError::ProcessFailed { .. }
        | ToolExecutionError::InvalidOutput(_)
        | ToolExecutionError::ExecutableChanged
        | ToolExecutionError::PersistentProcessSession(_) => "tool_execution_failed",
    }
}

fn tool_execution_failure_content(error: &ToolExecutionError) -> serde_json::Value {
    error
        .deterministic_failure_result()
        .map(|result| result.content)
        .unwrap_or_else(|| {
            serde_json::json!({
                "error": {
                    "code": tool_execution_error_code(error),
                    "message": "tool execution failed inside its assigned sandbox"
                }
            })
        })
}

#[cfg(test)]
mod runtime_policy_tests {
    use super::*;
    use agent_protocol::RuntimeExecutionPolicySnapshot;
    use chrono::Duration as ChronoDuration;

    const EXECUTION_V6_EXAMPLE: &str =
        include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");

    #[test]
    fn tool_context_uses_the_timeout_frozen_by_the_run() {
        let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
        command.schema_version = 10;
        command.skill_snapshots.clear();
        let mut policy = RuntimeExecutionPolicySnapshot {
            schema_version: 1,
            ..RuntimeExecutionPolicySnapshot::default()
        };
        policy.mcp_discovery.max_attempts_per_server = 1;
        policy.mcp_discovery.initial_retry_backoff_ms = 0;
        policy.tool_execution.timeout_ms = 1_234;
        policy.tool_execution.max_concurrent_tools = 1;
        command.runtime_policy = Some(policy);
        command.validate().unwrap();
        let mut worker = WorkerProcessor::new_with_incarnation(
            command.worker_id,
            command.worker_incarnation_id,
            vec![Placement::Cloud],
            1,
            "0.1.0".into(),
        )
        .unwrap();
        worker.accept(command.clone(), command.issued_at).unwrap();
        let workspace_base = tempfile::tempdir().unwrap();

        let context = worker
            .tool_execution_context(
                command.attempt_id,
                workspace_base.path(),
                command.issued_at + ChronoDuration::seconds(1),
            )
            .unwrap();

        assert_eq!(context.timeout, Duration::from_millis(1_234));
    }

    #[test]
    fn process_start_failure_keeps_a_distinct_worker_event_code() {
        let session_id = Uuid::now_v7();
        let error = ToolExecutionError::ProcessSessionStartFailed {
            session_id,
            reason: "private OS reason".into(),
        };

        assert_eq!(
            tool_execution_error_code(&error),
            "process_session_start_failed"
        );
        let content = tool_execution_failure_content(&error);
        assert_eq!(content["error"]["session_id"], session_id.to_string());
        assert_eq!(
            content["error"]["message"],
            "persistent process session could not be started"
        );
        assert!(
            !content.to_string().contains("private OS reason"),
            "Worker event content leaked the private execution reason"
        );
    }
}
