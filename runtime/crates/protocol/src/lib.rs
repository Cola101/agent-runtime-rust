use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use uuid::Uuid;

pub const RUN_QUEUED_SCHEMA_VERSION: u32 = 1;
pub const RUN_EXECUTION_SCHEMA_VERSION: u32 = 20;
pub const RUN_CANCELLATION_SCHEMA_VERSION: u32 = 2;
pub const RUN_STEERING_SCHEMA_VERSION: u32 = 1;
pub const RUN_STEERING_OUTCOME_SCHEMA_VERSION: u32 = 1;
pub const TOOL_APPROVAL_DECISION_SCHEMA_VERSION: u32 = 2;
pub const TOOL_RECONCILIATION_SCHEMA_VERSION: u32 = 1;
pub const TOOL_RECONCILIATION_MAX_CONTENT_BYTES: usize = 256 * 1024;
pub const MCP_INPUT_REQUIRED_SCHEMA_VERSION: u32 = 1;
pub const MCP_INPUT_RESOLUTION_SCHEMA_VERSION: u32 = 1;
pub const WORKER_HEARTBEAT_SCHEMA_VERSION: u32 = 2;
pub const RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION: u32 = 2;
pub const RUNTIME_INVOCATION_SCHEMA_VERSION: u32 = 1;
pub const EDGE_TASK_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPriority {
    Interactive,
    Batch,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Auto,
    Cloud,
    Edge,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunBudget {
    pub max_tokens: u64,
    pub max_cost_cents: u64,
    pub max_duration_seconds: u64,
}

/// Immutable customer-resource boundary selected before a Run enters Runtime
/// admission.
///
/// This is deliberately smaller than `RunExecutionCommand`: an embedding
/// adapter supplies and authenticates this context, while the Runtime allocates
/// Run/attempt/worker identity and the short-lived workload token afterwards.
/// Keeping it provider- and transport-neutral lets an in-process Java bridge,
/// a sidecar, a desktop client, and an edge node invoke the same Rust contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInvocationContext {
    pub schema_version: u32,
    pub tenant_id: Uuid,
    pub application_id: Uuid,
    /// Stable non-secret principal selected by the authenticating embedding
    /// adapter. Rotating bearer credentials are never persisted here.
    pub workload_identity_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
    pub model_policy_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RuntimeInvocationValidationError {
    #[error("unsupported Runtime invocation schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("Runtime invocation resource identity is incomplete")]
    IncompleteIdentity,
}

impl RuntimeInvocationContext {
    pub fn validate(&self) -> Result<(), RuntimeInvocationValidationError> {
        if self.schema_version != RUNTIME_INVOCATION_SCHEMA_VERSION {
            return Err(RuntimeInvocationValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if [
            self.tenant_id,
            self.application_id,
            self.workload_identity_id,
            self.workspace_id,
            self.agent_version_id,
            self.model_policy_id,
        ]
        .iter()
        .any(Uuid::is_nil)
        {
            return Err(RuntimeInvocationValidationError::IncompleteIdentity);
        }
        Ok(())
    }
}

/// Control-plane-authorized work addressed to one enrolled Edge Node
/// generation. Version 2 additionally binds the approved Enrollment and node
/// capability surface. It deliberately maps one task to one standalone Run;
/// persistent Session turns need a later contract that the embedded Runtime can
/// represent without changing identity at the execution boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeTaskClaims {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub enrollment_id: Uuid,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub capability_manifest_digest: String,
    pub required_capabilities: BTreeSet<String>,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_owner_epoch: u64,
    pub input: String,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EdgeTaskValidationError {
    #[error("unsupported edge task schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("edge task identity is incomplete")]
    IncompleteIdentity,
    #[error("edge task node generation must be positive")]
    InvalidNodeGeneration,
    #[error("edge task Enrollment binding is invalid")]
    InvalidEnrollmentBinding,
    #[error("edge task capability requirements are invalid")]
    InvalidCapabilities,
    #[error("edge task Workspace owner epoch must be positive")]
    InvalidOwnerEpoch,
    #[error("edge task Session identity is unsupported by schema v1")]
    UnsupportedSessionIdentity,
    #[error("edge task authority is expired or has an invalid lifetime")]
    InvalidLifetime,
    #[error("edge task input must be nonblank and at most 32000 bytes")]
    InvalidInput,
}

impl EdgeTaskClaims {
    pub fn validate_at(&self, now_unix_ms: i64) -> Result<(), EdgeTaskValidationError> {
        if self.schema_version != EDGE_TASK_SCHEMA_VERSION {
            return Err(EdgeTaskValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.task_id.is_nil()
            || self.enrollment_id.is_nil()
            || self.node_id.is_nil()
            || self.run_id.is_nil()
            || self.session_id.is_nil()
            || self.invocation.validate().is_err()
        {
            return Err(EdgeTaskValidationError::IncompleteIdentity);
        }
        if self.node_generation == 0 {
            return Err(EdgeTaskValidationError::InvalidNodeGeneration);
        }
        if !is_lower_hex_sha256(&self.capability_manifest_digest) {
            return Err(EdgeTaskValidationError::InvalidEnrollmentBinding);
        }
        if self.required_capabilities.is_empty()
            || self.required_capabilities.len() > 64
            || self
                .required_capabilities
                .iter()
                .any(|capability| !valid_edge_capability(capability))
        {
            return Err(EdgeTaskValidationError::InvalidCapabilities);
        }
        if self.workspace_owner_epoch == 0 {
            return Err(EdgeTaskValidationError::InvalidOwnerEpoch);
        }
        if self.session_id != self.run_id {
            return Err(EdgeTaskValidationError::UnsupportedSessionIdentity);
        }
        const MAX_LIFETIME_MS: i64 = 24 * 60 * 60 * 1_000;
        let lifetime_ms = self.expires_at_unix_ms.checked_sub(self.issued_at_unix_ms);
        if self.issued_at_unix_ms > now_unix_ms
            || self.expires_at_unix_ms <= now_unix_ms
            || self.expires_at_unix_ms <= self.issued_at_unix_ms
            || lifetime_ms.is_none_or(|lifetime| lifetime > MAX_LIFETIME_MS)
        {
            return Err(EdgeTaskValidationError::InvalidLifetime);
        }
        if self.input.trim().is_empty() || self.input.len() > 32_000 {
            return Err(EdgeTaskValidationError::InvalidInput);
        }
        Ok(())
    }
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_edge_capability(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b':' | b'_' | b'-' | b'/')
        })
}

/// The protocol-neutral execution semantics one Run freezes before admission.
///
/// Provider credentials, tenant routing and Tool grants live in their own
/// snapshots. This one binds the host-level scheduling decisions that used to
/// be constants in three different processes and could therefore drift when a
/// Run moved or resumed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExecutionPolicySnapshot {
    pub schema_version: u32,
    pub mcp_discovery: McpDiscoveryPolicySnapshot,
    pub model_failover: ModelFailoverPolicySnapshot,
    pub tool_execution: ToolExecutionPolicySnapshot,
    /// Provider-neutral model-context bounds. Disabled is the only valid value
    /// for policy schemas before v3, so an older scheduler cannot accidentally
    /// opt a Run into semantics it does not understand.
    #[serde(default)]
    pub context_compaction: ContextCompactionPolicySnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpDiscoveryPolicySnapshot {
    pub max_concurrent_servers: u8,
    pub per_server_timeout_ms: u64,
    pub total_timeout_ms: u64,
    /// Total safe discovery attempts for one server, including the first.
    /// Tool calls never consume this budget and are never automatically replayed.
    #[serde(default = "single_mcp_discovery_attempt")]
    pub max_attempts_per_server: u8,
    /// Initial delay between retryable discovery failures. Later delays double
    /// while the per-server deadline remains the hard outer bound.
    #[serde(default)]
    pub initial_retry_backoff_ms: u64,
}

const fn single_mcp_discovery_attempt() -> u8 {
    1
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFailoverPolicySnapshot {
    pub max_provider_attempts: u8,
    pub fallback_on: std::collections::BTreeSet<ModelErrorKind>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionPolicySnapshot {
    pub timeout_ms: u64,
    /// Maximum number of replay-safe Tool calls admitted in one ordered batch.
    /// Older policy schemas deserialize to one and therefore remain serial.
    #[serde(default = "single_concurrent_tool")]
    pub max_concurrent_tools: u8,
}

const fn single_concurrent_tool() -> u8 {
    1
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextCompactionPolicySnapshot {
    pub enabled: bool,
    pub trigger_bytes: u64,
    pub retain_bytes: u64,
    pub max_summary_tokens: u64,
}

impl Default for RuntimeExecutionPolicySnapshot {
    fn default() -> Self {
        Self {
            schema_version: 4,
            mcp_discovery: McpDiscoveryPolicySnapshot {
                max_concurrent_servers: 4,
                per_server_timeout_ms: 3_000,
                total_timeout_ms: 10_000,
                max_attempts_per_server: 2,
                initial_retry_backoff_ms: 100,
            },
            model_failover: ModelFailoverPolicySnapshot {
                max_provider_attempts: 8,
                fallback_on: std::collections::BTreeSet::from([
                    ModelErrorKind::RateLimited,
                    ModelErrorKind::Timeout,
                    ModelErrorKind::Unavailable,
                ]),
            },
            tool_execution: ToolExecutionPolicySnapshot {
                timeout_ms: 30_000,
                max_concurrent_tools: 4,
            },
            context_compaction: ContextCompactionPolicySnapshot::default(),
        }
    }
}

impl RuntimeExecutionPolicySnapshot {
    #[must_use]
    pub fn is_bounded_and_safe(&self) -> bool {
        matches!(self.schema_version, 1..=4)
            && (1..=16).contains(&self.mcp_discovery.max_concurrent_servers)
            && (1..=60_000).contains(&self.mcp_discovery.per_server_timeout_ms)
            && self.mcp_discovery.total_timeout_ms >= self.mcp_discovery.per_server_timeout_ms
            && self.mcp_discovery.total_timeout_ms <= 300_000
            && match self.schema_version {
                1 => {
                    self.mcp_discovery.max_attempts_per_server == 1
                        && self.mcp_discovery.initial_retry_backoff_ms == 0
                }
                2..=4 => {
                    (1..=4).contains(&self.mcp_discovery.max_attempts_per_server)
                        && self.mcp_discovery.initial_retry_backoff_ms <= 5_000
                }
                _ => false,
            }
            && (1..=8).contains(&self.model_failover.max_provider_attempts)
            && self.model_failover.fallback_on.iter().all(|kind| {
                matches!(
                    kind,
                    ModelErrorKind::RateLimited
                        | ModelErrorKind::Timeout
                        | ModelErrorKind::Unavailable
                )
            })
            && (1..=3_600_000).contains(&self.tool_execution.timeout_ms)
            && match self.schema_version {
                1..=3 => self.tool_execution.max_concurrent_tools == 1,
                4 => (1..=16).contains(&self.tool_execution.max_concurrent_tools),
                _ => false,
            }
            && match self.schema_version {
                1 | 2 => self.context_compaction == ContextCompactionPolicySnapshot::default(),
                3 | 4 if !self.context_compaction.enabled => {
                    self.context_compaction == ContextCompactionPolicySnapshot::default()
                }
                3 | 4 => {
                    (4_096..=67_108_864).contains(&self.context_compaction.trigger_bytes)
                        && (1_024..self.context_compaction.trigger_bytes)
                            .contains(&self.context_compaction.retain_bytes)
                        && (64..=8_192).contains(&self.context_compaction.max_summary_tokens)
                }
                _ => false,
            }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    Tokens,
    Cost,
    Duration,
}

impl RunBudget {
    const fn is_positive_and_finite(&self) -> bool {
        self.max_tokens > 0
            && self.max_cost_cents > 0
            && self.max_duration_seconds > 0
            && self.max_duration_seconds <= 86_400
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunQueuedCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
    pub model_policy_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub input: String,
    pub priority: RunPriority,
    pub placement: Placement,
    pub budget: RunBudget,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunQueuedValidationError {
    #[error("unsupported run queued schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("run input must not be blank")]
    BlankInput,
    #[error("run budgets must be finite and positive")]
    NonPositiveBudget,
    #[error("run duration must not exceed 86400 seconds")]
    DurationTooLong,
    #[error("run model policy id must not be nil")]
    MissingModelPolicy,
}

impl RunQueuedCommand {
    pub fn validate(&self) -> Result<(), RunQueuedValidationError> {
        if self.schema_version != RUN_QUEUED_SCHEMA_VERSION {
            return Err(RunQueuedValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.input.trim().is_empty() {
            return Err(RunQueuedValidationError::BlankInput);
        }
        if self.model_policy_id.is_nil() {
            return Err(RunQueuedValidationError::MissingModelPolicy);
        }
        if self.budget.max_tokens == 0
            || self.budget.max_cost_cents == 0
            || self.budget.max_duration_seconds == 0
        {
            return Err(RunQueuedValidationError::NonPositiveBudget);
        }
        if self.budget.max_duration_seconds > 86_400 {
            return Err(RunQueuedValidationError::DurationTooLong);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunExecutionCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    /// Immutable application boundary selected before admission.
    ///
    /// Added in v20. Older commands deserialize to nil for rolling read
    /// compatibility, but a v20 producer must never omit it: tenant identity
    /// alone cannot distinguish two customer applications that share one
    /// Runtime deployment.
    #[serde(default)]
    pub application_id: Uuid,
    /// Stable non-secret identity of the invoking workload. The short-lived
    /// signed token rotates independently and is never written to events or
    /// Checkpoints.
    #[serde(default)]
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
    pub model_policy_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    #[serde(default)]
    pub worker_incarnation_id: Uuid,
    pub owner_epoch: u64,
    pub fencing_token: Uuid,
    pub issued_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    pub workload_token: WorkloadToken,
    #[serde(default)]
    pub delegated_scopes: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub agent_instructions: String,
    #[serde(default)]
    pub model_policy_snapshot_base64: String,
    #[serde(default)]
    pub model_policy_digest: String,
    #[serde(default)]
    pub skill_snapshots: Vec<SkillSnapshot>,
    #[serde(default)]
    pub lineage: AgentLineage,
    #[serde(default)]
    pub subagent_roles: Vec<SubagentRole>,
    /// Per-Tool approval policy, decided by the tenant and carried here.
    ///
    /// Added in v8. It was a constant in the Worker before that, which meant
    /// every tenant granted a Tool got the same exemption and no tenant
    /// administrator could turn it off -- a decision that is theirs being made
    /// somewhere they cannot see or reach.
    ///
    /// A Tool absent from this map asks, so an older command and a command that
    /// simply does not mention a Tool both mean the safe thing.
    #[serde(default)]
    pub tool_approval_policies: std::collections::BTreeMap<String, AutoApproval>,
    /// Federated MCP servers this Run may reach (ADR-0040).
    ///
    /// Added in v9, and sealed exactly as a model Provider is: the Worker gets
    /// the endpoint and the namespace so it can name and route the tools, and
    /// never the credential. Unsealing happens at the egress hop, not on the
    /// machine that is executing a model's suggestions.
    ///
    /// Empty is the normal case and means no federation, so an older command and
    /// a tenant with no servers registered both mean the same safe thing.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSnapshot>,
    /// Effective host-level execution semantics, frozen before admission.
    ///
    /// Added in v10. `None` is accepted only for older commands so an upgrade
    /// remains read-compatible; a v10 Run without the snapshot is ambiguous and
    /// is rejected rather than inheriting whichever defaults this Worker has.
    #[serde(default)]
    pub runtime_policy: Option<RuntimeExecutionPolicySnapshot>,
    /// Completed turns inherited by a continuation child Run.
    ///
    /// This stays provider-neutral: the Worker adapter converts each turn into
    /// native user/assistant messages. Keeping it out of `agent_instructions`
    /// prevents conversation data from silently becoming higher-authority
    /// system input.
    #[serde(default)]
    pub subagent_history: Vec<SubagentConversationTurn>,
    /// Explicit lower-authority history import. This is repaired at admission
    /// and never used as an implicit fallback for malformed Checkpoint state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_import: Option<HistoryImport>,
    /// Authoritative completed-turn prefix for a root Session branch.
    ///
    /// Unlike `history_import`, this state is produced and integrity-checked by
    /// the Runtime itself. The Worker may expose it to the model as ordinary
    /// conversation context, but historical Tool calls are never scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_branch: Option<SessionBranchSnapshot>,
    pub input: String,
    pub budget: RunBudget,
}

/// One federated MCP server as the Worker sees it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum McpProtocolRevision {
    /// Stateful initialize/session protocol retained for compatibility.  It has
    /// no client-side reverse capabilities unless a future schema explicitly
    /// adds a separately recoverable legacy policy.
    #[default]
    #[serde(rename = "2025-06-18")]
    V2025_06_18,
    /// Stateless MCP core with multi round-trip requests (MRTR).
    #[serde(rename = "2026-07-28")]
    V2026_07_28,
}

impl McpProtocolRevision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2025_06_18 => "2025-06-18",
            Self::V2026_07_28 => "2026-07-28",
        }
    }
}

/// Authority the Runtime is prepared to exercise on behalf of an MCP server.
/// This is frozen per Run and intentionally separate from the server's own
/// advertised capabilities: an untrusted peer cannot grant itself client work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpClientCapability {
    Elicitation,
}

/// One user interaction requested by an MCP server in a stateless MRTR round.
/// Form data is deliberately restricted to non-sensitive, bounded structured
/// input; sensitive workflows belong in URL mode and never pass through the
/// Runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum McpElicitationRequest {
    Form {
        message: String,
        requested_schema: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Url {
        message: String,
        url: String,
        elicitation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpInputAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpInputResponse {
    pub action: McpInputAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpInputContinuation {
    pub round: u8,
    pub request_state: String,
    pub responses: std::collections::BTreeMap<String, McpInputResponse>,
}

/// Durable, Run-bound projection of an MCP `input_required` result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpInputRequired {
    pub schema_version: u32,
    pub input_id: Uuid,
    pub server_id: Uuid,
    pub server_name: String,
    pub tool_call_id: String,
    pub binding_digest: String,
    pub round: u8,
    /// Opaque server continuation token. It must be replayed byte-for-byte.
    pub request_state: String,
    pub requests: std::collections::BTreeMap<String, McpElicitationRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum McpInputRequiredValidationError {
    #[error("unsupported MCP input-required schema version")]
    UnsupportedSchemaVersion,
    #[error("MCP input-required binding is invalid")]
    InvalidBinding,
    #[error("MCP input-required round or opaque state is invalid")]
    InvalidContinuation,
    #[error("MCP input-required request set is malformed or unbounded")]
    InvalidRequests,
    #[error("MCP form elicitation attempted to request sensitive information")]
    SensitiveForm,
}

impl McpInputRequired {
    pub fn validate(&self) -> Result<(), McpInputRequiredValidationError> {
        if self.schema_version != MCP_INPUT_REQUIRED_SCHEMA_VERSION {
            return Err(McpInputRequiredValidationError::UnsupportedSchemaVersion);
        }
        if self.input_id.is_nil()
            || self.server_id.is_nil()
            || !portable_identifier(&self.server_name, 64)
            || self.tool_call_id.trim().is_empty()
            || self.tool_call_id.len() > 256
            || !is_sha256(&self.binding_digest)
        {
            return Err(McpInputRequiredValidationError::InvalidBinding);
        }
        if !(1..=10).contains(&self.round)
            || self.request_state.is_empty()
            || self.request_state.len() > 64 * 1024
        {
            return Err(McpInputRequiredValidationError::InvalidContinuation);
        }
        if self.requests.is_empty()
            || self.requests.len() > 8
            || serde_json::to_vec(&self.requests).map_or(true, |encoded| encoded.len() > 128 * 1024)
        {
            return Err(McpInputRequiredValidationError::InvalidRequests);
        }
        for (key, request) in &self.requests {
            if !portable_identifier(key, 128) || !request.is_bounded_and_safe()? {
                return Err(McpInputRequiredValidationError::InvalidRequests);
            }
        }
        Ok(())
    }
}

impl McpElicitationRequest {
    pub fn validate(&self) -> Result<(), McpInputRequiredValidationError> {
        if self.is_bounded_and_safe()? {
            Ok(())
        } else {
            Err(McpInputRequiredValidationError::InvalidRequests)
        }
    }

    fn is_bounded_and_safe(&self) -> Result<bool, McpInputRequiredValidationError> {
        let (message, meta) = match self {
            Self::Form {
                message,
                requested_schema,
                meta,
            } => {
                let Some(object) = requested_schema.as_object() else {
                    return Ok(false);
                };
                if object.get("type").and_then(Value::as_str) != Some("object")
                    || serde_json::to_vec(requested_schema)
                        .map_or(true, |encoded| encoded.len() > 32 * 1024)
                {
                    return Ok(false);
                }
                if object
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(|properties| {
                        properties.len() > 32
                            || properties.keys().any(|name| sensitive_form_property(name))
                    })
                {
                    return Err(McpInputRequiredValidationError::SensitiveForm);
                }
                (message, meta)
            }
            Self::Url {
                message,
                url,
                elicitation_id,
                meta,
            } => {
                if !url.starts_with("https://")
                    || url.len() > 2_048
                    || url.chars().any(char::is_whitespace)
                    || elicitation_id.trim().is_empty()
                    || elicitation_id.len() > 256
                {
                    return Ok(false);
                }
                (message, meta)
            }
        };
        Ok(!message.trim().is_empty()
            && message.len() <= 2_048
            && meta.as_ref().is_none_or(|meta| {
                serde_json::to_vec(meta).is_ok_and(|encoded| encoded.len() <= 16 * 1024)
            }))
    }
}

fn sensitive_form_property(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace(['-', '.'], "_");
    [
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "private_key",
        "credential",
    ]
    .iter()
    .any(|term| normalized == *term || normalized.ends_with(&format!("_{term}")))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpInputResolutionCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub input_id: Uuid,
    pub input_version: u32,
    pub binding_digest: String,
    pub responses: std::collections::BTreeMap<String, McpInputResponse>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum McpInputResolutionValidationError {
    #[error("unsupported MCP input resolution schema version")]
    UnsupportedSchemaVersion,
    #[error("MCP input resolution identity or binding is invalid")]
    InvalidBinding,
    #[error("MCP input resolution validity window is invalid")]
    InvalidValidityWindow,
    #[error("MCP input resolution does not answer the exact pending request set")]
    InvalidResponses,
}

impl McpInputResolutionCommand {
    pub fn validate_for(
        &self,
        pending: &McpInputRequired,
    ) -> Result<(), McpInputResolutionValidationError> {
        if self.schema_version != MCP_INPUT_RESOLUTION_SCHEMA_VERSION {
            return Err(McpInputResolutionValidationError::UnsupportedSchemaVersion);
        }
        if self.message_id.is_nil()
            || self.tenant_id.is_nil()
            || self.run_id.is_nil()
            || self.attempt_id.is_nil()
            || self.worker_id.is_nil()
            || self.worker_incarnation_id.is_nil()
            || self.input_id != pending.input_id
            || self.input_version != 1
            || self.binding_digest != pending.binding_digest
        {
            return Err(McpInputResolutionValidationError::InvalidBinding);
        }
        if self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > chrono::Duration::minutes(5)
        {
            return Err(McpInputResolutionValidationError::InvalidValidityWindow);
        }
        if self.responses.len() != pending.requests.len()
            || self.responses.keys().ne(pending.requests.keys())
            || serde_json::to_vec(&self.responses)
                .map_or(true, |encoded| encoded.len() > 128 * 1024)
        {
            return Err(McpInputResolutionValidationError::InvalidResponses);
        }
        for (key, response) in &self.responses {
            let request = &pending.requests[key];
            let valid = match response.action {
                McpInputAction::Decline | McpInputAction::Cancel => response.content.is_none(),
                McpInputAction::Accept => match request {
                    McpElicitationRequest::Form {
                        requested_schema, ..
                    } => response
                        .content
                        .as_ref()
                        .is_some_and(|content| form_content_matches(requested_schema, content)),
                    McpElicitationRequest::Url { .. } => response.content.is_none(),
                },
            } && response.meta.as_ref().is_none_or(|meta| {
                serde_json::to_vec(meta).is_ok_and(|encoded| encoded.len() <= 16 * 1024)
            });
            if !valid {
                return Err(McpInputResolutionValidationError::InvalidResponses);
            }
        }
        Ok(())
    }
}

fn form_content_matches(schema: &Value, content: &Value) -> bool {
    let (Some(schema), Some(content)) = (schema.as_object(), content.as_object()) else {
        return false;
    };
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .any(|name| name.as_str().is_none_or(|name| !content.contains_key(name)))
        })
    {
        return false;
    }
    content.iter().all(|(name, value)| {
        properties
            .get(name)
            .is_some_and(|field| match field.get("type").and_then(Value::as_str) {
                Some("string") => value.is_string(),
                Some("boolean") => value.is_boolean(),
                Some("number") => value.is_number(),
                Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
                Some("array") => value.is_array(),
                Some("object") => value.is_object(),
                Some("null") => value.is_null(),
                _ => false,
            })
    })
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpServerSnapshot {
    pub server_id: Uuid,
    /// Namespace in qualified tool names: `mcp:<name>/<tool>`.
    pub name: String,
    /// The only host the federation client may reach for this server.
    pub endpoint: String,
    /// Sealed. Base64 of the envelope, opaque here and opened at the egress hop.
    #[serde(default)]
    pub credential_envelope_base64: String,
    /// A required server is part of the Run's accepted capability contract.
    /// Discovery failure therefore stops before model egress rather than
    /// silently presenting a smaller Tool catalog.
    #[serde(default)]
    pub required: bool,
    /// Operator-owned replay semantics keyed by the server-local Tool name.
    ///
    /// This is deliberately separate from MCP Tool annotations: remote output
    /// is untrusted input and cannot lower the Runtime's side-effect boundary.
    /// An absent entry always means `ToolEffect::Unknown`.
    #[serde(default)]
    pub tool_effect_overrides: std::collections::BTreeMap<String, ToolEffect>,
    /// Exact wire revision selected before Run admission.  Older commands
    /// decode to the legacy revision but cannot carry modern capabilities.
    #[serde(default)]
    pub protocol_revision: McpProtocolRevision,
    /// Run-frozen client authority available to MRTR input requests.
    #[serde(default)]
    pub client_capabilities: std::collections::BTreeSet<McpClientCapability>,
}

impl McpServerSnapshot {
    /// The name shape, re-checked here rather than trusted from the control
    /// plane. Validating a contract on receipt is the point of having one; a
    /// name carrying `/` or `:` could make one server's tool resolve as
    /// another's, and the Worker is the party that would act on it.
    fn has_usable_namespace(&self) -> bool {
        let bytes = self.name.as_bytes();
        // Written as one expression with no `||` at the top level. The first
        // version mixed && and || and, because && binds tighter, read as
        // "(everything) or (starts with a digit)" -- so `1/b` was accepted and
        // could have forged a qualified name. The hostile list in the test now
        // carries a digit-leading case for that reason.
        matches!(bytes.first(), Some(first)
            if first.is_ascii_lowercase() || first.is_ascii_digit())
            && bytes.len() <= 64
            && bytes.iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
            })
    }

    fn scope(&self) -> String {
        format!("tool:mcp:{}", self.name)
    }

    fn capability_scope(&self, capability: McpClientCapability) -> String {
        match capability {
            McpClientCapability::Elicitation => format!("mcp:elicitation:{}", self.name),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentLineage {
    pub root_run_id: Uuid,
    pub parent_run_id: Option<Uuid>,
    pub delegation_id: Option<Uuid>,
    pub depth: u8,
    pub role: String,
}

impl AgentLineage {
    fn valid_for(&self, run_id: Uuid) -> bool {
        if self.depth == 0 {
            return self.root_run_id == run_id
                && self.parent_run_id.is_none()
                && self.delegation_id.is_none()
                && self.role == "primary";
        }
        self.depth <= 3
            && !self.root_run_id.is_nil()
            && self.root_run_id != run_id
            && self
                .parent_run_id
                .is_some_and(|parent_run_id| !parent_run_id.is_nil() && parent_run_id != run_id)
            && self
                .delegation_id
                .is_some_and(|delegation_id| !delegation_id.is_nil())
            && self.role != "primary"
            && portable_identifier(&self.role, 80)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentRole {
    pub name: String,
    pub instructions: String,
    #[serde(default)]
    pub delegated_scopes: std::collections::BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentSpawnMode {
    /// Compatibility path: the spawning Tool call receives the terminal child
    /// result. New interactive agents should use `Async`.
    #[default]
    Inline,
    /// Return a durable handle immediately and observe the child through the
    /// explicit agent lifecycle Tools.
    Async,
}

pub const SUBAGENT_HISTORY_MAX_TURNS: usize = 128;
pub const SUBAGENT_HISTORY_MAX_BYTES: usize = 2 * 1024 * 1024;

/// One completed turn in the durable conversation owned by an asynchronous
/// subagent handle. `message_sequence` is caller acceptance order while
/// `activation_ordinal` is actual execution order; an interrupt can make those
/// orders differ.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentConversationTurn {
    pub activation_ordinal: u64,
    pub message_sequence: u64,
    pub child_run_id: Uuid,
    pub input: String,
    pub result: SubagentResultDelivery,
}

/// Immutable provenance for a new persistent handle derived from a completed
/// prefix of another handle. The receipt is provider-neutral and contains no
/// live child, mailbox or process state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentForkReceipt {
    pub tool_call_id: String,
    pub tool_binding_digest: String,
    pub source_agent_id: Uuid,
    pub source_generation: u64,
    pub through_activation_ordinal: u64,
    pub source_history_digest: String,
    pub agent_id: Uuid,
    pub generation: u64,
    pub role: String,
    pub budget: RunBudget,
}

impl SubagentForkReceipt {
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.tool_call_id.trim().is_empty()
            && self.tool_call_id.len() <= 256
            && is_sha256(&self.tool_binding_digest)
            && !self.source_agent_id.is_nil()
            && self.source_generation > 0
            && is_sha256(&self.source_history_digest)
            && !self.agent_id.is_nil()
            && self.agent_id != self.source_agent_id
            && self.generation > 0
            && self.role != "primary"
            && portable_identifier(&self.role, 80)
            && self.budget.is_positive_and_finite()
    }
}

/// Immutable audit receipt for replacing a stable handle's active history head
/// with a completed prefix. The superseded generation remains addressable; the
/// new generation fences every continuation binding created under the old head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentRollbackReceipt {
    pub tool_call_id: String,
    pub tool_binding_digest: String,
    pub agent_id: Uuid,
    pub from_generation: u64,
    pub generation: u64,
    pub through_activation_ordinal: u64,
    pub previous_history_digest: String,
    pub restored_history_digest: String,
}

impl SubagentRollbackReceipt {
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.tool_call_id.trim().is_empty()
            && self.tool_call_id.len() <= 256
            && is_sha256(&self.tool_binding_digest)
            && !self.agent_id.is_nil()
            && self.from_generation > 0
            && self
                .from_generation
                .checked_add(1)
                .is_some_and(|generation| generation == self.generation)
            && is_sha256(&self.previous_history_digest)
            && is_sha256(&self.restored_history_digest)
            && self.previous_history_digest != self.restored_history_digest
    }
}

#[must_use]
pub fn subagent_conversation_history_digest(history: &[SubagentConversationTurn]) -> String {
    let material = serde_json::to_vec(&("agent-runtime-subagent-history-v1", history))
        .expect("subagent history digest material is serializable");
    hex::encode(Sha256::digest(material))
}

#[must_use]
pub fn subagent_conversation_history_is_well_formed(history: &[SubagentConversationTurn]) -> bool {
    valid_subagent_conversation_history(history)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentSpawnRequest {
    pub tool_call_id: String,
    pub delegation_id: Uuid,
    pub role: String,
    pub input: String,
    pub budget: RunBudget,
    pub binding_digest: String,
    #[serde(default)]
    pub mode: SubagentSpawnMode,
    /// Snapshot of completed turns at activation time. Queued input is first
    /// accepted as an intent and receives this final snapshot when promoted.
    #[serde(default)]
    pub conversation_history: Vec<SubagentConversationTurn>,
}

impl SubagentSpawnRequest {
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.tool_call_id.trim().is_empty()
            && self.tool_call_id.len() <= 256
            && !self.delegation_id.is_nil()
            && self.role != "primary"
            && portable_identifier(&self.role, 80)
            && !self.input.trim().is_empty()
            && self.input.len() <= 32_000
            && self.budget.is_positive_and_finite()
            && is_sha256(&self.binding_digest)
            && valid_subagent_conversation_history(&self.conversation_history)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillSnapshot {
    pub schema_version: u32,
    pub application_id: Uuid,
    pub skill_version_id: Uuid,
    pub name: String,
    pub semantic_version: String,
    pub description: String,
    pub instructions: String,
    pub tool_names: Vec<String>,
    pub supported_platforms: Vec<String>,
    pub min_runtime_version: String,
    pub artifact_digest: String,
    pub signing_key_id: String,
    pub signature: String,
}

impl SkillSnapshot {
    fn canonical_bytes(&self, tenant_id: Uuid) -> Vec<u8> {
        let canonical = std::collections::BTreeMap::from([
            ("application_id", serde_json::json!(self.application_id)),
            ("description", serde_json::json!(self.description)),
            ("instructions", serde_json::json!(self.instructions)),
            (
                "min_runtime_version",
                serde_json::json!(self.min_runtime_version),
            ),
            ("name", serde_json::json!(self.name)),
            ("schema_version", serde_json::json!(self.schema_version)),
            ("semantic_version", serde_json::json!(self.semantic_version)),
            ("skill_version_id", serde_json::json!(self.skill_version_id)),
            (
                "supported_platforms",
                serde_json::json!(self.supported_platforms),
            ),
            ("tenant_id", serde_json::json!(tenant_id)),
            ("tool_names", serde_json::json!(self.tool_names)),
        ]);
        serde_json::to_vec(&canonical).expect("skill snapshot canonical form is serializable")
    }

    #[must_use]
    /// The digest this snapshot's contents imply.
    ///
    /// Exposed so a caller that changes a snapshot can recompute it from the one
    /// implementation rather than mirroring the canonical field list, which is
    /// how the two would drift.
    pub fn expected_artifact_digest(&self, tenant_id: Uuid) -> String {
        hex::encode(Sha256::digest(self.canonical_bytes(tenant_id)))
    }

    pub fn artifact_digest_matches(&self, tenant_id: Uuid) -> bool {
        is_sha256(&self.artifact_digest)
            && self.expected_artifact_digest(tenant_id) == self.artifact_digest
    }

    fn validate(&self, tenant_id: Uuid) -> bool {
        self.schema_version == 1
            && !self.application_id.is_nil()
            && !self.skill_version_id.is_nil()
            && portable_identifier(&self.name, 120)
            && semantic_version(&self.semantic_version)
            && !self.description.trim().is_empty()
            && self.description.len() <= 500
            && !self.instructions.trim().is_empty()
            && self.instructions.len() <= 32_000
            && sorted_unique(&self.tool_names, 32)
            && self
                .tool_names
                .iter()
                .all(|name| skill_tool_name(name, 120))
            && sorted_unique(&self.supported_platforms, 8)
            && !self.supported_platforms.is_empty()
            && self.supported_platforms.iter().all(|platform| {
                matches!(
                    platform.as_str(),
                    "darwin-arm64" | "linux-arm64" | "linux-x86_64"
                )
            })
            && semantic_version(&self.min_runtime_version)
            && portable_identifier(&self.signing_key_id, 128)
            && base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&self.signature)
                .is_ok_and(|signature| signature.len() == 64)
            && self.artifact_digest_matches(tenant_id)
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkloadToken(String);

impl WorkloadToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_well_formed(&self) -> bool {
        !self.0.trim().is_empty() && self.0.len() <= 8192 && self.0.split('.').count() == 3
    }
}

impl fmt::Debug for WorkloadToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkloadToken[REDACTED]")
    }
}

pub const WORKLOAD_IDENTITY_RENEWAL_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadIdentityRenewalCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub owner_epoch: u64,
    pub fencing_token: Uuid,
    pub generation: u64,
    pub issued_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    pub workload_token: WorkloadToken,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkloadIdentityRenewalValidationError {
    #[error("unsupported workload identity renewal schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("workload identity renewal binding is invalid")]
    InvalidBinding,
    #[error("workload identity renewal window is invalid")]
    InvalidWindow,
    #[error("workload identity renewal token is malformed")]
    InvalidToken,
}

impl WorkloadIdentityRenewalCommand {
    pub fn validate(&self) -> Result<(), WorkloadIdentityRenewalValidationError> {
        if self.schema_version != WORKLOAD_IDENTITY_RENEWAL_SCHEMA_VERSION {
            return Err(
                WorkloadIdentityRenewalValidationError::UnsupportedSchemaVersion(
                    self.schema_version,
                ),
            );
        }
        if self.message_id.is_nil()
            || self.tenant_id.is_nil()
            || self.run_id.is_nil()
            || self.attempt_id.is_nil()
            || self.worker_id.is_nil()
            || self.worker_incarnation_id.is_nil()
            || self.owner_epoch == 0
            || self.fencing_token.is_nil()
            || self.generation == 0
        {
            return Err(WorkloadIdentityRenewalValidationError::InvalidBinding);
        }
        let lifetime = self.lease_expires_at.signed_duration_since(self.issued_at);
        if lifetime <= chrono::Duration::zero() || lifetime > chrono::Duration::minutes(5) {
            return Err(WorkloadIdentityRenewalValidationError::InvalidWindow);
        }
        if !self.workload_token.is_well_formed() {
            return Err(WorkloadIdentityRenewalValidationError::InvalidToken);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunExecutionValidationError {
    #[error("unsupported execution schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("execution owner epoch must be positive")]
    NonPositiveOwnerEpoch,
    #[error("execution lease must expire after it is issued")]
    InvalidLeaseWindow,
    #[error("execution input must not be blank")]
    BlankInput,
    #[error("execution budgets must be finite and positive")]
    InvalidBudget,
    #[error("execution model policy id must not be nil")]
    MissingModelPolicy,
    #[error("execution workload token is malformed")]
    InvalidWorkloadToken,
    #[error("v20 execution invocation identity is incomplete")]
    InvalidInvocationIdentity,
    #[error("execution delegated scopes are malformed or exceed the contract limit")]
    InvalidDelegatedScopes,
    #[error("v3 execution agent instructions are blank or exceed 32000 bytes")]
    InvalidAgentInstructions,
    #[error("v4 execution model policy snapshot is missing, oversized, or digest mismatched")]
    InvalidModelPolicySnapshot,
    #[error("v5 execution Skill snapshot is missing, malformed, or digest mismatched")]
    InvalidSkillSnapshot,
    #[error("v6 execution agent lineage is missing or inconsistent")]
    InvalidAgentLineage,
    #[error("v7 execution subagent role catalog is malformed or exceeds current authority")]
    InvalidSubagentRoles,
    #[error("v2 execution must target one worker incarnation")]
    MissingWorkerIncarnation,
    #[error("tool approval policies are only carried from v8 onward")]
    InvalidToolApprovalPolicies,
    #[error("v9 federated MCP servers are malformed, undelegated, or carried by an older schema")]
    InvalidMcpServers,
    #[error("v10 execution runtime policy is missing, malformed, or carried by an older schema")]
    InvalidRuntimePolicy,
    #[error(
        "v11 MCP availability and discovery retry policy cannot be carried by an older execution schema"
    )]
    InvalidMcpAvailabilityPolicy,
    #[error("v17 parallel Tool policy is malformed or carried by an older execution schema")]
    InvalidParallelToolPolicy,
    #[error(
        "v18 MCP Tool effect overrides are malformed, undeclared, or carried by an older execution schema"
    )]
    InvalidMcpToolEffectOverrides,
    #[error(
        "v19 MCP protocol revision or client capability policy is malformed, undelegated, or carried by an older execution schema"
    )]
    InvalidMcpProtocolPolicy,
    #[error("v12 subagent conversation history is malformed or carried by an older schema")]
    InvalidSubagentHistory,
    #[error("v15 explicit history import is malformed, ambiguous, or carried by an older schema")]
    InvalidHistoryImport,
    #[error("v16 root Session branch is malformed, ambiguous, or carried by an older schema")]
    InvalidSessionBranch,
}

impl RunExecutionCommand {
    pub fn validate(&self) -> Result<(), RunExecutionValidationError> {
        if !(1..=RUN_EXECUTION_SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(RunExecutionValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.schema_version >= 20
            && [
                self.tenant_id,
                self.application_id,
                self.workload_identity_id,
                self.run_id,
                self.session_id,
                self.workspace_id,
                self.agent_version_id,
                self.attempt_id,
                self.worker_id,
                self.worker_incarnation_id,
                self.fencing_token,
            ]
            .iter()
            .any(Uuid::is_nil)
        {
            return Err(RunExecutionValidationError::InvalidInvocationIdentity);
        }
        if self.schema_version >= 2 && self.worker_incarnation_id.is_nil() {
            return Err(RunExecutionValidationError::MissingWorkerIncarnation);
        }
        if self.owner_epoch == 0 {
            return Err(RunExecutionValidationError::NonPositiveOwnerEpoch);
        }
        if self.lease_expires_at <= self.issued_at {
            return Err(RunExecutionValidationError::InvalidLeaseWindow);
        }
        if self.input.trim().is_empty() {
            return Err(RunExecutionValidationError::BlankInput);
        }
        if self.model_policy_id.is_nil() {
            return Err(RunExecutionValidationError::MissingModelPolicy);
        }
        if !self.workload_token.is_well_formed() {
            return Err(RunExecutionValidationError::InvalidWorkloadToken);
        }
        if self.delegated_scopes.len() > 128
            || self
                .delegated_scopes
                .iter()
                .any(|scope| scope.trim().is_empty() || scope.len() > 128)
        {
            return Err(RunExecutionValidationError::InvalidDelegatedScopes);
        }
        if self.schema_version >= 3
            && (self.agent_instructions.trim().is_empty() || self.agent_instructions.len() > 32_000)
        {
            return Err(RunExecutionValidationError::InvalidAgentInstructions);
        }
        if self.schema_version >= 4 && !self.valid_model_policy_snapshot() {
            return Err(RunExecutionValidationError::InvalidModelPolicySnapshot);
        }
        if (self.schema_version == 5
            || (self.schema_version >= 6 && !self.skill_snapshots.is_empty()))
            && !self.valid_skill_snapshots()
        {
            return Err(RunExecutionValidationError::InvalidSkillSnapshot);
        }
        if self.schema_version >= 6 && !self.lineage.valid_for(self.run_id) {
            return Err(RunExecutionValidationError::InvalidAgentLineage);
        }
        if (self.schema_version < 7 && !self.subagent_roles.is_empty())
            || (self.schema_version >= 7 && !self.valid_subagent_roles())
        {
            return Err(RunExecutionValidationError::InvalidSubagentRoles);
        }
        // A command claiming an older schema must not carry a v8 field: that is
        // how a downgraded command would smuggle an exemption past a Worker
        // that believes it is speaking the policy-free contract.
        if self.schema_version < 8 && !self.tool_approval_policies.is_empty() {
            return Err(RunExecutionValidationError::InvalidToolApprovalPolicies);
        }
        // The same downgrade guard v8 has: a command claiming an older schema
        // must not carry v9 servers, or federation reaches a Worker that
        // believes it is speaking a contract without any in it.
        if (self.schema_version < 9 && !self.mcp_servers.is_empty())
            || (self.schema_version >= 9 && !self.valid_mcp_servers())
        {
            return Err(RunExecutionValidationError::InvalidMcpServers);
        }
        if (self.schema_version < 18
            && self
                .mcp_servers
                .iter()
                .any(|server| !server.tool_effect_overrides.is_empty()))
            || (self.schema_version >= 18 && !self.valid_mcp_tool_effect_overrides())
        {
            return Err(RunExecutionValidationError::InvalidMcpToolEffectOverrides);
        }
        if (self.schema_version < 19
            && self.mcp_servers.iter().any(|server| {
                server.protocol_revision != McpProtocolRevision::V2025_06_18
                    || !server.client_capabilities.is_empty()
            }))
            || (self.schema_version >= 19 && !self.valid_mcp_protocol_policy())
        {
            return Err(RunExecutionValidationError::InvalidMcpProtocolPolicy);
        }
        if (self.schema_version < 10 && self.runtime_policy.is_some())
            || (self.schema_version >= 10
                && !self
                    .runtime_policy
                    .as_ref()
                    .is_some_and(RuntimeExecutionPolicySnapshot::is_bounded_and_safe))
        {
            return Err(RunExecutionValidationError::InvalidRuntimePolicy);
        }
        let policy_schema = self
            .runtime_policy
            .as_ref()
            .map(|policy| policy.schema_version)
            .unwrap_or_default();
        if (self.schema_version < 11
            && (policy_schema >= 2 || self.mcp_servers.iter().any(|server| server.required)))
            || ((11..13).contains(&self.schema_version) && policy_schema != 2)
            || ((13..17).contains(&self.schema_version) && policy_schema != 3)
        {
            return Err(RunExecutionValidationError::InvalidMcpAvailabilityPolicy);
        }
        if (self.schema_version < 17 && policy_schema >= 4)
            || (self.schema_version >= 17 && policy_schema != 4)
        {
            return Err(RunExecutionValidationError::InvalidParallelToolPolicy);
        }
        let carries_typed_subagent_history = self
            .subagent_history
            .iter()
            .any(|turn| !turn.result.transcript.is_empty());
        let has_legacy_subagent_turn = self
            .subagent_history
            .iter()
            .any(|turn| turn.result.transcript.is_empty());
        if (self.schema_version < 12 && !self.subagent_history.is_empty())
            || ((12..14).contains(&self.schema_version) && carries_typed_subagent_history)
            || (self.schema_version >= 12
                && (!valid_subagent_conversation_history(&self.subagent_history)
                    || (!self.subagent_history.is_empty() && self.lineage.depth == 0)))
            || (self.schema_version >= 14 && has_legacy_subagent_turn)
        {
            return Err(RunExecutionValidationError::InvalidSubagentHistory);
        }
        let root_execution = self.lineage.depth == 0;
        if (self.schema_version < 16 && self.session_branch.is_some())
            || (self.schema_version >= 16
                && ((root_execution
                    && !self
                        .session_branch
                        .as_ref()
                        .is_some_and(SessionBranchSnapshot::is_well_formed))
                    || (!root_execution && self.session_branch.is_some())
                    || (self.session_branch.is_some()
                        && (self.history_import.is_some() || !self.subagent_history.is_empty()))))
        {
            return Err(RunExecutionValidationError::InvalidSessionBranch);
        }
        if (self.schema_version < 15 && self.history_import.is_some())
            || (self.history_import.is_some() && !self.subagent_history.is_empty())
            || self
                .history_import
                .as_ref()
                .is_some_and(|history| repair_imported_history(history).is_err())
        {
            return Err(RunExecutionValidationError::InvalidHistoryImport);
        }
        if !self.budget.is_positive_and_finite() {
            return Err(RunExecutionValidationError::InvalidBudget);
        }
        Ok(())
    }

    fn valid_mcp_servers(&self) -> bool {
        if self.mcp_servers.len() > 16 {
            return false;
        }
        let mut seen = std::collections::BTreeSet::new();
        self.mcp_servers.iter().all(|server| {
            !server.server_id.is_nil()
                && server.has_usable_namespace()
                && server.endpoint.len() <= 2_048
                && !server.endpoint.trim().is_empty()
                && server.credential_envelope_base64.len() <= 16 * 1024
                // A server nobody delegated is either a mistake or a
                // pre-authorisation waiting for a scope change to activate it.
                && self.delegated_scopes.contains(&server.scope())
                && seen.insert(server.name.clone())
        })
    }

    fn valid_mcp_tool_effect_overrides(&self) -> bool {
        let declared_tools = self
            .skill_snapshots
            .iter()
            .flat_map(|skill| skill.tool_names.iter())
            .collect::<std::collections::BTreeSet<_>>();
        let override_count = self
            .mcp_servers
            .iter()
            .map(|server| server.tool_effect_overrides.len())
            .sum::<usize>();
        override_count <= 512
            && self.mcp_servers.iter().all(|server| {
                server.tool_effect_overrides.keys().all(|tool_name| {
                    portable_identifier(tool_name, 120)
                        && declared_tools.contains(&format!("mcp:{}/{tool_name}", server.name))
                })
            })
    }

    fn valid_mcp_protocol_policy(&self) -> bool {
        self.mcp_servers.iter().all(|server| {
            if server.protocol_revision == McpProtocolRevision::V2025_06_18 {
                return server.client_capabilities.is_empty();
            }
            !server.client_capabilities.is_empty()
                && server.client_capabilities.iter().all(|capability| {
                    self.delegated_scopes
                        .contains(&server.capability_scope(*capability))
                })
        })
    }

    fn valid_subagent_roles(&self) -> bool {
        if self.subagent_roles.len() > 16
            || (self.lineage.depth >= 3 && !self.subagent_roles.is_empty())
        {
            return false;
        }
        let mut names = std::collections::BTreeSet::new();
        self.subagent_roles.iter().all(|role| {
            role.name != "primary"
                && portable_identifier(&role.name, 80)
                && names.insert(role.name.as_str())
                && !role.instructions.trim().is_empty()
                && role.instructions.len() <= 16_000
                && role.delegated_scopes.len() <= 128
                && role.delegated_scopes.iter().all(|scope| {
                    !scope.trim().is_empty()
                        && scope.len() <= 128
                        && self.delegated_scopes.contains(scope)
                })
        })
    }

    fn valid_skill_snapshots(&self) -> bool {
        if self.skill_snapshots.is_empty() || self.skill_snapshots.len() > 16 {
            return false;
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        self.skill_snapshots.iter().all(|skill| {
            ids.insert(skill.skill_version_id)
                && names.insert(skill.name.as_str())
                && (self.schema_version < 20 || skill.application_id == self.application_id)
                && skill.validate(self.tenant_id)
        })
    }

    fn valid_model_policy_snapshot(&self) -> bool {
        if self.model_policy_digest.len() != 64
            || !self
                .model_policy_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return false;
        }
        let Ok(snapshot) =
            base64::engine::general_purpose::STANDARD.decode(&self.model_policy_snapshot_base64)
        else {
            return false;
        };
        if snapshot.is_empty() || snapshot.len() > 256 * 1024 {
            return false;
        }
        let digest = hex::encode(Sha256::digest(&snapshot));
        if digest != self.model_policy_digest {
            return false;
        }
        let Ok(snapshot) = serde_json::from_slice::<Value>(&snapshot) else {
            return false;
        };
        snapshot.get("schema_version").and_then(Value::as_u64) == Some(1)
            && snapshot.get("routing").and_then(Value::as_str).is_some()
            && snapshot
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|candidates| !candidates.is_empty() && candidates.len() <= 8)
    }
}

fn sorted_unique(values: &[String], maximum: usize) -> bool {
    values.len() <= maximum
        && values
            .windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str())
}

/// A Tool name a Skill may declare.
///
/// Either a native Tool's portable identifier, or a federated Tool's qualified
/// `mcp:<server>/<tool>` (ADR-0040). Federated names were impossible before
/// this: `portable_identifier` forbids `:` and `/`, so a Skill declaring one
/// made the whole snapshot invalid and the Run was refused for asking for
/// something the platform is meant to support.
///
/// Both halves must themselves be portable identifiers, so exactly one `:` and
/// one `/` can appear and neither half can be empty. That is what keeps a name
/// from parsing as a different server than it names.
fn skill_tool_name(value: &str, maximum: usize) -> bool {
    if value.len() > maximum {
        return false;
    }
    let Some(qualified) = value.strip_prefix("mcp:") else {
        return portable_identifier(value, maximum);
    };
    let Some((server, tool)) = qualified.split_once('/') else {
        return false;
    };
    portable_identifier(server, 64) && portable_identifier(tool, 128)
}

fn portable_identifier(value: &str, maximum: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= maximum
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn semantic_version(value: &str) -> bool {
    let (core, prerelease_valid) = value
        .split_once('-')
        .map_or((value, true), |(core, suffix)| {
            (
                core,
                !suffix.is_empty()
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')),
            )
        });
    let mut parts = core.split('.');
    let valid_part = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    valid_part(parts.next().unwrap_or_default())
        && valid_part(parts.next().unwrap_or_default())
        && valid_part(parts.next().unwrap_or_default())
        && parts.next().is_none()
        && prerelease_valid
        && value.len() <= 64
}

#[cfg(test)]
mod skill_validation_tests {
    use super::{portable_identifier, semantic_version};

    #[test]
    fn portable_skill_identifiers_match_the_control_plane_contract_at_digit_boundaries() {
        assert!(portable_identifier("1review", 120));
        assert!(portable_identifier("review1", 120));
    }

    #[test]
    fn semantic_versions_reject_an_empty_prerelease_suffix() {
        assert!(!semantic_version("1.0.0-"));
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCancellationCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    #[serde(default)]
    pub worker_incarnation_id: Uuid,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunCancellationValidationError {
    #[error("unsupported run cancellation schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("run cancellation validity window is invalid")]
    InvalidValidityWindow,
    #[error("run cancellation reason must not be blank")]
    BlankReason,
    #[error("v2 run cancellation must target one worker incarnation")]
    MissingWorkerIncarnation,
}

impl RunCancellationCommand {
    pub fn validate(&self) -> Result<(), RunCancellationValidationError> {
        if !matches!(self.schema_version, 1 | RUN_CANCELLATION_SCHEMA_VERSION) {
            return Err(RunCancellationValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.schema_version == RUN_CANCELLATION_SCHEMA_VERSION
            && self.worker_incarnation_id.is_nil()
        {
            return Err(RunCancellationValidationError::MissingWorkerIncarnation);
        }
        if self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > chrono::Duration::minutes(5)
        {
            return Err(RunCancellationValidationError::InvalidValidityWindow);
        }
        if self.reason.trim().is_empty() {
            return Err(RunCancellationValidationError::BlankReason);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSteeringCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub steering_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub input: String,
    pub input_digest: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSteeringTarget {
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSteeringRequest {
    pub input: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunSteeringValidationError {
    #[error("unsupported run steering schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("run steering identity is invalid")]
    InvalidIdentity,
    #[error("run steering input must contain at most 32768 UTF-8 bytes")]
    InvalidInput,
    #[error("run steering input digest is invalid")]
    InvalidInputDigest,
    #[error("run steering validity window is invalid")]
    InvalidValidityWindow,
}

impl RunSteeringCommand {
    #[must_use]
    pub fn new(
        message_id: Uuid,
        steering_id: Uuid,
        target: RunSteeringTarget,
        request: RunSteeringRequest,
    ) -> Self {
        let input_digest = hex::encode(Sha256::digest(request.input.as_bytes()));
        Self {
            schema_version: RUN_STEERING_SCHEMA_VERSION,
            message_id,
            steering_id,
            tenant_id: target.tenant_id,
            run_id: target.run_id,
            attempt_id: target.attempt_id,
            worker_id: target.worker_id,
            worker_incarnation_id: target.worker_incarnation_id,
            input: request.input,
            input_digest,
            issued_at: request.issued_at,
            expires_at: request.expires_at,
        }
    }

    pub fn validate(&self) -> Result<(), RunSteeringValidationError> {
        if self.schema_version != RUN_STEERING_SCHEMA_VERSION {
            return Err(RunSteeringValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.message_id.is_nil()
            || self.steering_id.is_nil()
            || self.tenant_id.is_nil()
            || self.run_id.is_nil()
            || self.attempt_id.is_nil()
            || self.worker_id.is_nil()
            || self.worker_incarnation_id.is_nil()
        {
            return Err(RunSteeringValidationError::InvalidIdentity);
        }
        if self.input.trim().is_empty() || self.input.len() > 32 * 1024 {
            return Err(RunSteeringValidationError::InvalidInput);
        }
        if !is_sha256(&self.input_digest)
            || self.input_digest != hex::encode(Sha256::digest(self.input.as_bytes()))
        {
            return Err(RunSteeringValidationError::InvalidInputDigest);
        }
        if self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > chrono::Duration::minutes(5)
        {
            return Err(RunSteeringValidationError::InvalidValidityWindow);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSteeringOutcome {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub steering_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
    pub input_digest: String,
    pub outcome: String,
    pub reason: String,
    pub occurred_at: DateTime<Utc>,
}

impl RunSteeringOutcome {
    #[must_use]
    pub fn rejected(
        command: &RunSteeringCommand,
        worker_id: Uuid,
        worker_incarnation_id: Uuid,
        reason: impl Into<String>,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: RUN_STEERING_OUTCOME_SCHEMA_VERSION,
            message_id: command.steering_id,
            steering_id: command.steering_id,
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id,
            worker_incarnation_id,
            input_digest: command.input_digest.clone(),
            outcome: "rejected".to_string(),
            reason: reason.into(),
            occurred_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    AllowOnce,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolApprovalDecisionCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    #[serde(default)]
    pub worker_incarnation_id: Uuid,
    pub approval_id: Uuid,
    pub approval_version: u32,
    pub binding_digest: String,
    pub decision: ToolApprovalDecision,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolApprovalDecisionValidationError {
    #[error("unsupported tool approval decision schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("tool approval decision identity must be complete")]
    MissingIdentity,
    #[error("tool approval decision version must be at least 2")]
    InvalidApprovalVersion,
    #[error("tool approval binding digest must be lowercase SHA-256")]
    InvalidBindingDigest,
    #[error("tool approval decision validity window is invalid")]
    InvalidValidityWindow,
    #[error("v2 tool approval decision must target one worker incarnation")]
    MissingWorkerIncarnation,
}

impl ToolApprovalDecisionCommand {
    pub fn validate(&self) -> Result<(), ToolApprovalDecisionValidationError> {
        if !matches!(
            self.schema_version,
            1 | TOOL_APPROVAL_DECISION_SCHEMA_VERSION
        ) {
            return Err(
                ToolApprovalDecisionValidationError::UnsupportedSchemaVersion(self.schema_version),
            );
        }
        if self.schema_version == TOOL_APPROVAL_DECISION_SCHEMA_VERSION
            && self.worker_incarnation_id.is_nil()
        {
            return Err(ToolApprovalDecisionValidationError::MissingWorkerIncarnation);
        }
        if self.message_id.is_nil()
            || self.tenant_id.is_nil()
            || self.run_id.is_nil()
            || self.attempt_id.is_nil()
            || self.worker_id.is_nil()
            || self.approval_id.is_nil()
        {
            return Err(ToolApprovalDecisionValidationError::MissingIdentity);
        }
        if self.approval_version < 2 {
            return Err(ToolApprovalDecisionValidationError::InvalidApprovalVersion);
        }
        if self.binding_digest.len() != 64
            || !self
                .binding_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ToolApprovalDecisionValidationError::InvalidBindingDigest);
        }
        if self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > chrono::Duration::minutes(5)
        {
            return Err(ToolApprovalDecisionValidationError::InvalidValidityWindow);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ToolReconciliationDecision {
    Applied {
        content: serde_json::Value,
        is_error: bool,
    },
    NotApplied,
    Unresolved,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolReconciliationCommand {
    pub schema_version: u32,
    pub reconciliation_id: Uuid,
    pub version: u32,
    pub tenant_id: Uuid,
    pub source_run_id: Uuid,
    pub source_terminal_event_id: Uuid,
    pub tool_call_id: String,
    pub binding_digest: String,
    pub operator_id: String,
    pub decision: ToolReconciliationDecision,
    pub continuation_input: Option<String>,
    pub issued_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolReconciliationValidationError {
    #[error("unsupported Tool reconciliation schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("Tool reconciliation identity must be complete")]
    MissingIdentity,
    #[error("Tool reconciliation version must start at 1")]
    InvalidVersion,
    #[error("Tool reconciliation binding digest must be lowercase SHA-256")]
    InvalidBindingDigest,
    #[error("Tool reconciliation text fields are invalid")]
    InvalidText,
    #[error("Tool reconciliation result content is too large")]
    InvalidContent,
    #[error("Tool reconciliation continuation input does not match its decision")]
    InvalidContinuationInput,
}

impl ToolReconciliationCommand {
    pub fn validate(&self) -> Result<(), ToolReconciliationValidationError> {
        if self.schema_version != TOOL_RECONCILIATION_SCHEMA_VERSION {
            return Err(ToolReconciliationValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.reconciliation_id.is_nil()
            || self.tenant_id.is_nil()
            || self.source_run_id.is_nil()
            || self.source_terminal_event_id.is_nil()
        {
            return Err(ToolReconciliationValidationError::MissingIdentity);
        }
        if self.version == 0 {
            return Err(ToolReconciliationValidationError::InvalidVersion);
        }
        if !is_sha256(&self.binding_digest) {
            return Err(ToolReconciliationValidationError::InvalidBindingDigest);
        }
        if self.tool_call_id.trim().is_empty()
            || self.tool_call_id.len() > 256
            || self.operator_id.trim().is_empty()
            || self.operator_id.len() > 256
        {
            return Err(ToolReconciliationValidationError::InvalidText);
        }
        if let ToolReconciliationDecision::Applied { content, .. } = &self.decision
            && serde_json::to_vec(content).map_or(true, |encoded| {
                encoded.len() > TOOL_RECONCILIATION_MAX_CONTENT_BYTES
            })
        {
            return Err(ToolReconciliationValidationError::InvalidContent);
        }
        let valid_continuation = match (&self.decision, &self.continuation_input) {
            (ToolReconciliationDecision::Unresolved, None) => true,
            (
                ToolReconciliationDecision::Applied { .. } | ToolReconciliationDecision::NotApplied,
                Some(input),
            ) => !input.trim().is_empty() && input.len() <= 32_000,
            _ => false,
        };
        if !valid_continuation {
            return Err(ToolReconciliationValidationError::InvalidContinuationInput);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveRunAssignment {
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub workspace_id: Uuid,
    pub owner_epoch: u64,
    pub fencing_token: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerHeartbeat {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub worker_id: Uuid,
    #[serde(default)]
    pub incarnation_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub placements: Vec<Placement>,
    pub capacity: u32,
    pub active_runs: u32,
    #[serde(default)]
    pub active_assignments: Vec<ActiveRunAssignment>,
    pub runtime_version: String,
    #[serde(default = "default_true")]
    pub accepting_work: bool,
    #[serde(default)]
    pub draining_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub drain_deadline: Option<DateTime<Utc>>,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WorkerHeartbeatValidationError {
    #[error("unsupported worker heartbeat schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("worker capacity must be positive")]
    NonPositiveCapacity,
    #[error("worker active runs must not exceed capacity")]
    CapacityOvercommitted,
    #[error("worker assignment details must not exceed active run count")]
    AssignmentCountMismatch,
    #[error("worker must advertise at least one placement")]
    MissingPlacement,
    #[error("worker runtime version must not be blank")]
    BlankRuntimeVersion,
    #[error("v2 heartbeat must identify one worker incarnation")]
    MissingIncarnation,
    #[error("worker heartbeat drain metadata must match its admission state")]
    InconsistentDrainState,
    #[error("worker drain deadline must be after draining started")]
    InvalidDrainWindow,
}

impl WorkerHeartbeat {
    pub fn validate(&self) -> Result<(), WorkerHeartbeatValidationError> {
        if !matches!(self.schema_version, 1 | WORKER_HEARTBEAT_SCHEMA_VERSION) {
            return Err(WorkerHeartbeatValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.schema_version == WORKER_HEARTBEAT_SCHEMA_VERSION && self.incarnation_id.is_nil() {
            return Err(WorkerHeartbeatValidationError::MissingIncarnation);
        }
        if self.capacity == 0 {
            return Err(WorkerHeartbeatValidationError::NonPositiveCapacity);
        }
        if self.active_runs > self.capacity {
            return Err(WorkerHeartbeatValidationError::CapacityOvercommitted);
        }
        if self.active_assignments.len() > self.active_runs as usize {
            return Err(WorkerHeartbeatValidationError::AssignmentCountMismatch);
        }
        if self.placements.is_empty() {
            return Err(WorkerHeartbeatValidationError::MissingPlacement);
        }
        if self.runtime_version.trim().is_empty() {
            return Err(WorkerHeartbeatValidationError::BlankRuntimeVersion);
        }
        match (
            self.accepting_work,
            self.draining_since,
            self.drain_deadline,
        ) {
            (true, None, None) => {}
            (false, Some(draining_since), Some(drain_deadline)) => {
                if draining_since > self.occurred_at || drain_deadline <= draining_since {
                    return Err(WorkerHeartbeatValidationError::InvalidDrainWindow);
                }
            }
            _ => return Err(WorkerHeartbeatValidationError::InconsistentDrainState),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunExecutionAccepted {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    #[serde(default)]
    pub worker_incarnation_id: Uuid,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunExecutionAcceptedValidationError {
    #[error("unsupported execution acceptance schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("execution attempt id must not be nil")]
    NilAttemptId,
    #[error("v2 execution acceptance must identify one worker incarnation")]
    MissingWorkerIncarnation,
}

impl RunExecutionAccepted {
    pub fn validate(&self) -> Result<(), RunExecutionAcceptedValidationError> {
        if !matches!(
            self.schema_version,
            1 | RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION
        ) {
            return Err(
                RunExecutionAcceptedValidationError::UnsupportedSchemaVersion(self.schema_version),
            );
        }
        if self.attempt_id.is_nil() {
            return Err(RunExecutionAcceptedValidationError::NilAttemptId);
        }
        if self.schema_version == RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION
            && self.worker_incarnation_id.is_nil()
        {
            return Err(RunExecutionAcceptedValidationError::MissingWorkerIncarnation);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Opaque continuation state returned by one provider protocol.
///
/// `data` is durable runtime state, not user-visible reasoning.  The origin
/// binding prevents an adapter from replaying it to a different provider or
/// model merely because both happen to accept similarly shaped JSON.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPrivateState {
    pub provider_id: String,
    pub protocol: String,
    pub model: String,
    pub format: String,
    pub data: String,
}

impl ProviderPrivateState {
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.provider_id.trim().is_empty()
            && self.provider_id.len() <= 128
            && !self.protocol.trim().is_empty()
            && self.protocol.len() <= 64
            && !self.model.trim().is_empty()
            && self.model.len() <= 256
            && !self.format.trim().is_empty()
            && self.format.len() <= 128
            && !self.data.is_empty()
            && self.data.len() <= 1_048_576
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        source: String,
    },
    Audio {
        media_type: String,
        source: String,
    },
    ToolResult {
        tool_call_id: String,
        content: Value,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        arguments: Value,
    },
    /// Provider-authored summary is intentionally distinct from opaque state.
    /// The latter is retained for same-provider continuation but must never be
    /// rendered as assistant text or copied into public runtime events.
    Reasoning {
        summary: Vec<String>,
        private_state: Option<ProviderPrivateState>,
    },
    Refusal {
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

pub const SESSION_HISTORY_MAX_TURNS: usize = 128;
pub const SESSION_HISTORY_MAX_BYTES: usize = 2 * 1024 * 1024;

/// One immutable completed root Turn. The transcript starts with the user
/// input for this Turn and contains every assistant Tool Call and bound Tool
/// Result through the terminal assistant message. It never contains System
/// authority or messages inherited from earlier Turns.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConversationTurn {
    pub turn_ordinal: u64,
    pub run_id: Uuid,
    pub transcript: Vec<Message>,
    pub digest: String,
}

impl SessionConversationTurn {
    #[must_use]
    pub fn new(turn_ordinal: u64, run_id: Uuid, transcript: Vec<Message>) -> Self {
        let mut turn = Self {
            turn_ordinal,
            run_id,
            transcript,
            digest: String::new(),
        };
        turn.digest = turn.calculate_digest();
        turn
    }

    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.digest == self.calculate_digest()
    }

    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.turn_ordinal > 0
            && !self.run_id.is_nil()
            && valid_subagent_transcript(&self.transcript)
            && self
                .transcript
                .last()
                .is_some_and(|message| message.role == Role::Assistant)
            && self.verify_digest()
    }

    fn calculate_digest(&self) -> String {
        let material = serde_json::to_vec(&(
            "agent-runtime-session-turn-v1",
            self.turn_ordinal,
            self.run_id,
            &self.transcript,
        ))
        .expect("Session Turn digest material is serializable");
        hex::encode(Sha256::digest(material))
    }
}

/// The exact effective head used to start one root Run. `session_id` remains
/// stable outside this value; sibling branches have distinct `branch_id`s and
/// Rollback advances `generation` without mutating archived history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBranchSnapshot {
    pub branch_id: Uuid,
    pub generation: u64,
    pub history: Vec<SessionConversationTurn>,
    pub history_digest: String,
}

impl SessionBranchSnapshot {
    #[must_use]
    pub fn new(branch_id: Uuid, generation: u64, history: Vec<SessionConversationTurn>) -> Self {
        let history_digest = session_conversation_history_digest(&history);
        Self {
            branch_id,
            generation,
            history,
            history_digest,
        }
    }

    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.branch_id.is_nil()
            && self.generation > 0
            && valid_session_conversation_history(&self.history)
            && self.history_digest == session_conversation_history_digest(&self.history)
    }
}

#[must_use]
pub fn session_conversation_history_digest(history: &[SessionConversationTurn]) -> String {
    let material = serde_json::to_vec(&("agent-runtime-session-history-v1", history))
        .expect("Session history digest material is serializable");
    hex::encode(Sha256::digest(material))
}

fn valid_session_conversation_history(history: &[SessionConversationTurn]) -> bool {
    if history.len() > SESSION_HISTORY_MAX_TURNS
        || serde_json::to_vec(history)
            .map_or(true, |encoded| encoded.len() > SESSION_HISTORY_MAX_BYTES)
    {
        return false;
    }
    let mut run_ids = std::collections::BTreeSet::new();
    history.iter().enumerate().all(|(index, turn)| {
        turn.turn_ordinal == u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1)
            && run_ids.insert(turn.run_id)
            && turn.is_well_formed()
    })
}

pub const HISTORY_IMPORT_MAX_MESSAGES: usize = 1_024;
pub const HISTORY_IMPORT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Why a caller is explicitly importing model-visible history. This is not
/// used for authoritative Checkpoint restore, whose malformed state remains a
/// hard failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryImportSource {
    External,
    Truncated,
}

/// Raw, lower-authority conversation history supplied through an explicit
/// import boundary. The Worker repairs only Tool pairing; it never promotes an
/// imported message to System authority or executes a historical Tool call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryImport {
    pub schema_version: u32,
    pub source: HistoryImportSource,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryRepairReport {
    pub schema_version: u32,
    pub source: HistoryImportSource,
    pub source_digest: String,
    pub repaired_digest: String,
    pub inserted_missing_results: u32,
    pub dropped_orphan_results: u32,
    pub dropped_duplicate_results: u32,
    pub moved_results: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepairedHistory {
    pub messages: Vec<Message>,
    pub report: HistoryRepairReport,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HistoryRepairError {
    #[error("unsupported history import schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("imported history is empty or exceeds its message/byte limit")]
    InvalidSize,
    #[error("imported history must not contain System messages")]
    SystemAuthority,
    #[error("imported history contains malformed role content")]
    InvalidRoleContent,
    #[error("imported history repeats Tool Call id {0}")]
    DuplicateToolCallId(String),
    #[error("repaired imported history is not a valid model transcript")]
    InvalidRepairedHistory,
}

/// Repairs only the replay shape of explicitly imported lower-authority
/// history. Results are moved only to a unique earlier Tool Call, missing
/// results become explicit synthetic errors, and results with no owner are
/// dropped. No Tool is invoked by this function.
pub fn repair_imported_history(
    history: &HistoryImport,
) -> Result<RepairedHistory, HistoryRepairError> {
    if history.schema_version != 1 {
        return Err(HistoryRepairError::UnsupportedSchemaVersion(
            history.schema_version,
        ));
    }
    if history.messages.is_empty()
        || history.messages.len() > HISTORY_IMPORT_MAX_MESSAGES
        || serde_json::to_vec(&history.messages)
            .map_or(true, |encoded| encoded.len() > HISTORY_IMPORT_MAX_BYTES)
    {
        return Err(HistoryRepairError::InvalidSize);
    }

    let mut call_positions = std::collections::BTreeMap::<String, usize>::new();
    let mut call_order = std::collections::BTreeMap::<usize, Vec<String>>::new();
    let mut result_candidates = std::collections::BTreeMap::<String, Vec<(usize, Message)>>::new();
    for (message_index, message) in history.messages.iter().enumerate() {
        if message.content.is_empty() || message.role == Role::System {
            return Err(if message.role == Role::System {
                HistoryRepairError::SystemAuthority
            } else {
                HistoryRepairError::InvalidRoleContent
            });
        }
        match message.role {
            Role::System => return Err(HistoryRepairError::SystemAuthority),
            Role::User => {
                if message.content.iter().any(|part| {
                    matches!(
                        part,
                        ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. }
                    )
                }) {
                    return Err(HistoryRepairError::InvalidRoleContent);
                }
            }
            Role::Assistant => {
                for part in &message.content {
                    match part {
                        ContentPart::ToolResult { .. } => {
                            return Err(HistoryRepairError::InvalidRoleContent);
                        }
                        ContentPart::ToolCall {
                            tool_call_id, name, ..
                        } => {
                            if tool_call_id.trim().is_empty()
                                || tool_call_id.len() > 256
                                || name.trim().is_empty()
                                || name.len() > 256
                            {
                                return Err(HistoryRepairError::InvalidRoleContent);
                            }
                            if call_positions
                                .insert(tool_call_id.clone(), message_index)
                                .is_some()
                            {
                                return Err(HistoryRepairError::DuplicateToolCallId(
                                    tool_call_id.clone(),
                                ));
                            }
                            call_order
                                .entry(message_index)
                                .or_default()
                                .push(tool_call_id.clone());
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                let [
                    ContentPart::ToolResult {
                        tool_call_id,
                        content: _,
                    },
                ] = message.content.as_slice()
                else {
                    return Err(HistoryRepairError::InvalidRoleContent);
                };
                if tool_call_id.trim().is_empty() || tool_call_id.len() > 256 {
                    return Err(HistoryRepairError::InvalidRoleContent);
                }
                result_candidates
                    .entry(tool_call_id.clone())
                    .or_default()
                    .push((message_index, message.clone()));
            }
        }
    }

    let mut inserted_missing_results = 0_u32;
    let mut dropped_orphan_results = 0_u32;
    let mut dropped_duplicate_results = 0_u32;
    let mut moved_results = 0_u32;
    let mut selected_results = std::collections::BTreeMap::<String, Message>::new();
    for (tool_call_id, candidates) in &result_candidates {
        let Some(call_index) = call_positions.get(tool_call_id).copied() else {
            dropped_orphan_results = dropped_orphan_results
                .saturating_add(u32::try_from(candidates.len()).unwrap_or(u32::MAX));
            continue;
        };
        let mut after_call = candidates
            .iter()
            .filter(|(result_index, _)| *result_index > call_index);
        let Some((selected_index, selected)) = after_call.next() else {
            dropped_orphan_results = dropped_orphan_results
                .saturating_add(u32::try_from(candidates.len()).unwrap_or(u32::MAX));
            continue;
        };
        let before_or_at = candidates
            .iter()
            .filter(|(result_index, _)| *result_index <= call_index)
            .count();
        dropped_orphan_results =
            dropped_orphan_results.saturating_add(u32::try_from(before_or_at).unwrap_or(u32::MAX));
        dropped_duplicate_results = dropped_duplicate_results
            .saturating_add(u32::try_from(after_call.count()).unwrap_or(u32::MAX));
        let call_offset = call_order
            .get(&call_index)
            .and_then(|calls| calls.iter().position(|id| id == tool_call_id))
            .unwrap_or_default();
        if *selected_index != call_index.saturating_add(call_offset).saturating_add(1) {
            moved_results = moved_results.saturating_add(1);
        }
        selected_results.insert(tool_call_id.clone(), selected.clone());
    }

    let mut repaired =
        Vec::with_capacity(history.messages.len().saturating_add(call_positions.len()));
    for (message_index, message) in history.messages.iter().enumerate() {
        if message.role == Role::Tool {
            continue;
        }
        repaired.push(message.clone());
        for tool_call_id in call_order.get(&message_index).into_iter().flatten() {
            if let Some(result) = selected_results.remove(tool_call_id) {
                repaired.push(result);
            } else {
                inserted_missing_results = inserted_missing_results.saturating_add(1);
                repaired.push(Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        content: serde_json::json!({
                            "error": {
                                "kind": "history_repair_missing_tool_result",
                                "message": "Tool result was unavailable in the imported history.",
                                "synthetic": true
                            }
                        }),
                    }],
                });
            }
        }
    }
    if !valid_subagent_transcript(&repaired)
        || serde_json::to_vec(&repaired)
            .map_or(true, |encoded| encoded.len() > HISTORY_IMPORT_MAX_BYTES)
    {
        return Err(HistoryRepairError::InvalidRepairedHistory);
    }

    let source_material = serde_json::to_vec(&(
        "agent-runtime-history-import-v1",
        history.source,
        &history.messages,
    ))
    .expect("bounded imported history is serializable");
    let repaired_material = serde_json::to_vec(&(
        "agent-runtime-history-repaired-v1",
        history.source,
        &repaired,
    ))
    .expect("bounded repaired history is serializable");
    Ok(RepairedHistory {
        messages: repaired,
        report: HistoryRepairReport {
            schema_version: 1,
            source: history.source,
            source_digest: hex::encode(Sha256::digest(source_material)),
            repaired_digest: hex::encode(Sha256::digest(repaired_material)),
            inserted_missing_results,
            dropped_orphan_results,
            dropped_duplicate_results,
            moved_results,
        },
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPolicy {
    Minimal,
    Balanced,
    Thorough,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub output_schema: Option<Value>,
    pub reasoning: ReasoningPolicy,
    pub max_output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Authentication,
    Billing,
    RateLimited,
    Timeout,
    Protocol,
    ContextOverflow,
    CapabilityMismatch,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    TextDelta {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Reasoning {
        summary: Vec<String>,
        private_state: Option<ProviderPrivateState>,
    },
    Refusal {
        text: String,
    },
    /// An audit observation emitted when opaque continuation state cannot be
    /// replayed to the selected provider. It deliberately contains no state.
    PrivateStateOmitted {
        origin_provider_id: String,
        target_provider_id: String,
        format: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_micros: u64,
    },
    Completed {
        reason: ModelFinishReason,
    },
    Failed {
        kind: ModelErrorKind,
        retryable: bool,
        message: String,
    },
}

impl ModelStreamEvent {
    /// Whether forwarding this event means provider output was committed to
    /// the caller. Audit-only observations must not disable safe fallback.
    #[must_use]
    pub const fn commits_provider_output(&self) -> bool {
        !matches!(self, Self::PrivateStateOmitted { .. })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Indeterminate,
}

impl RunStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Indeterminate
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Suspended => "suspended",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Pure,
    Idempotent,
    NonIdempotent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxClass {
    RestrictedContainer,
    Kata,
    TrustedNative,
    /// Runs on someone else's machine (ADR-0040).
    ///
    /// Its own variant rather than reusing a container class, because those
    /// name containment this platform applies and none of it applies here.
    /// Anything reading a descriptor to decide how a Tool is confined must be
    /// able to tell "confined by us" from "not ours to confine", and a borrowed
    /// variant would answer that question wrongly.
    Federated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub effect: ToolEffect,
    pub approval: ApprovalMode,
    pub sandbox: SandboxClass,
    pub implementation_digest: String,
    pub required_scopes: std::collections::BTreeSet<String>,
}

/// Narrow, per-Tool exemptions from the approval gate.
///
/// Kept as an explicit enum rather than a boolean so that adding a second
/// exemption forces a decision about which Tool it applies to. The variant is
/// part of the policy snapshot the approval ledger records, so an exemption is
/// auditable rather than invisible.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoApproval {
    /// Every call asks. The default, and what every Tool had before this
    /// existed.
    #[default]
    Never,
    /// A shell command that is provably read-only may run without asking. The
    /// container already prevents writes outside the Workspace, all network and
    /// the credential directories, so a command that cannot write has nothing
    /// left for a person to approve.
    ProvablyReadOnlyShellCommand,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolExecutionRequest {
    pub call: ToolCall,
    pub effect: ToolEffect,
    pub sandbox: SandboxClass,
    pub binding_digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolApprovalRequest {
    pub approval_id: Uuid,
    pub execution: ToolExecutionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_snapshot: Option<ToolApprovalPolicySnapshot>,
    #[serde(default)]
    pub policy_digest: String,
    #[serde(default)]
    pub session_scope_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolApprovalPolicySnapshot {
    pub tool_name: String,
    pub effect: ToolEffect,
    pub approval: ApprovalMode,
    pub sandbox: SandboxClass,
    pub implementation_digest: String,
    pub required_scopes: std::collections::BTreeSet<String>,
    /// Recorded so the ledger shows what exemption was in force, not just that
    /// an approval was or was not asked for.
    #[serde(default)]
    pub auto_approval: AutoApproval,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointSnapshot {
    pub run_id: Uuid,
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub status: RunStatus,
    pub sequence: u64,
    #[serde(with = "base64_checkpoint_state")]
    pub state: Vec<u8>,
    pub digest: String,
}

mod base64_checkpoint_state {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EncodedState {
        Base64(String),
        LegacyBytes(Vec<u8>),
    }

    pub fn serialize<S>(state: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(state))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match EncodedState::deserialize(deserializer)? {
            EncodedState::Base64(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(serde::de::Error::custom),
            EncodedState::LegacyBytes(bytes) => Ok(bytes),
        }
    }
}

impl CheckpointSnapshot {
    #[must_use]
    pub fn new(
        run_id: Uuid,
        tenant_id: Uuid,
        session_id: Uuid,
        attempt_id: Uuid,
        status: RunStatus,
        sequence: u64,
        state: Vec<u8>,
    ) -> Self {
        let digest = Self::calculate_digest(
            run_id, tenant_id, session_id, attempt_id, status, sequence, &state,
        );
        Self {
            run_id,
            tenant_id,
            session_id,
            attempt_id,
            status,
            sequence,
            state,
            digest,
        }
    }

    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.digest
            == Self::calculate_digest(
                self.run_id,
                self.tenant_id,
                self.session_id,
                self.attempt_id,
                self.status,
                self.sequence,
                &self.state,
            )
    }

    fn calculate_digest(
        run_id: Uuid,
        tenant_id: Uuid,
        session_id: Uuid,
        attempt_id: Uuid,
        status: RunStatus,
        sequence: u64,
        state: &[u8],
    ) -> String {
        let material = serde_json::to_vec(&(
            run_id, tenant_id, session_id, attempt_id, status, sequence, state,
        ))
        .expect("checkpoint digest material is serializable");
        hex::encode(Sha256::digest(material))
    }
}

pub const RUN_CHECKPOINT_SCHEMA_VERSION: u32 = 2;
pub const RUN_RECOVERY_SCHEMA_VERSION: u32 = 3;
pub const SUBAGENT_RESULT_MAX_BYTES: usize = 256 * 1024;
pub const SUBAGENT_TRANSCRIPT_MAX_BYTES: usize = 1024 * 1024;
pub const INLINE_CHECKPOINT_MAX_BYTES: usize = 512 * 1024;
pub const CHECKPOINT_MAX_UNCOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPayloadEncoding {
    #[default]
    Identity,
    Zstd,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCheckpointPublished {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub tenant_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub attempt_id: Uuid,
    pub owner_epoch: u64,
    pub fencing_token: Uuid,
    pub sequence: u64,
    pub status: RunStatus,
    pub kernel_digest: String,
    pub tool_catalog_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<String>,
    #[serde(default)]
    pub payload_encoding: CheckpointPayloadEncoding,
    pub payload_digest: String,
    #[serde(default)]
    pub stored_payload_digest: String,
    #[serde(default)]
    pub uncompressed_size: u64,
    #[serde(default)]
    pub stored_size: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedRunCheckpoint {
    pub message: RunCheckpointPublished,
    pub external_payload: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunCheckpointValidationError {
    #[error("unsupported checkpoint schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("checkpoint identity or lease is invalid")]
    InvalidIdentity,
    #[error("checkpoint digest is invalid")]
    InvalidDigest,
    #[error("checkpoint payload is invalid")]
    InvalidPayload,
}

impl RunCheckpointPublished {
    #[must_use]
    pub fn new(
        snapshot: &CheckpointSnapshot,
        owner_epoch: u64,
        fencing_token: Uuid,
        tool_catalog_digest: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        let payload = serde_json::to_vec(snapshot).expect("checkpoint snapshot is serializable");
        Self {
            schema_version: 1,
            message_id: Uuid::now_v7(),
            tenant_id: snapshot.tenant_id,
            run_id: snapshot.run_id,
            session_id: snapshot.session_id,
            attempt_id: snapshot.attempt_id,
            owner_epoch,
            fencing_token,
            sequence: snapshot.sequence,
            status: snapshot.status,
            kernel_digest: snapshot.digest.clone(),
            tool_catalog_digest,
            payload_base64: Some(base64::engine::general_purpose::STANDARD.encode(&payload)),
            payload_ref: None,
            payload_encoding: CheckpointPayloadEncoding::Identity,
            payload_digest: hex::encode(Sha256::digest(&payload)),
            stored_payload_digest: hex::encode(Sha256::digest(&payload)),
            uncompressed_size: payload.len() as u64,
            stored_size: payload.len() as u64,
            created_at,
        }
    }

    pub fn prepare_v2(
        snapshot: &CheckpointSnapshot,
        owner_epoch: u64,
        fencing_token: Uuid,
        tool_catalog_digest: String,
        created_at: DateTime<Utc>,
    ) -> Result<PreparedRunCheckpoint, RunCheckpointValidationError> {
        let payload = serde_json::to_vec(snapshot)
            .map_err(|_| RunCheckpointValidationError::InvalidPayload)?;
        if payload.is_empty() || payload.len() > CHECKPOINT_MAX_UNCOMPRESSED_BYTES {
            return Err(RunCheckpointValidationError::InvalidPayload);
        }
        let stored = zstd::bulk::compress(&payload, 3)
            .map_err(|_| RunCheckpointValidationError::InvalidPayload)?;
        let payload_digest = hex::encode(Sha256::digest(&payload));
        let stored_payload_digest = hex::encode(Sha256::digest(&stored));
        let is_inline = stored.len() <= INLINE_CHECKPOINT_MAX_BYTES;
        let message = Self {
            schema_version: RUN_CHECKPOINT_SCHEMA_VERSION,
            message_id: Uuid::now_v7(),
            tenant_id: snapshot.tenant_id,
            run_id: snapshot.run_id,
            session_id: snapshot.session_id,
            attempt_id: snapshot.attempt_id,
            owner_epoch,
            fencing_token,
            sequence: snapshot.sequence,
            status: snapshot.status,
            kernel_digest: snapshot.digest.clone(),
            tool_catalog_digest,
            payload_base64: is_inline
                .then(|| base64::engine::general_purpose::STANDARD.encode(&stored)),
            payload_ref: (!is_inline)
                .then(|| format!("checkpoint://sha256/{stored_payload_digest}")),
            payload_encoding: CheckpointPayloadEncoding::Zstd,
            payload_digest,
            stored_payload_digest,
            uncompressed_size: payload.len() as u64,
            stored_size: stored.len() as u64,
            created_at,
        };
        message.validate_metadata()?;
        if is_inline {
            message.validate()?;
        }
        Ok(PreparedRunCheckpoint {
            message,
            external_payload: (!is_inline).then_some(stored),
        })
    }

    pub fn validate(&self) -> Result<(), RunCheckpointValidationError> {
        if !matches!(self.schema_version, 1 | RUN_CHECKPOINT_SCHEMA_VERSION) {
            return Err(RunCheckpointValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.message_id.is_nil()
            || self.tenant_id.is_nil()
            || self.run_id.is_nil()
            || self.session_id.is_nil()
            || self.attempt_id.is_nil()
            || self.owner_epoch == 0
            || self.fencing_token.is_nil()
            || self.status.is_terminal()
        {
            return Err(RunCheckpointValidationError::InvalidIdentity);
        }
        if !is_sha256(&self.kernel_digest)
            || !is_sha256(&self.tool_catalog_digest)
            || !is_sha256(&self.payload_digest)
        {
            return Err(RunCheckpointValidationError::InvalidDigest);
        }
        self.validate_metadata()?;
        if self.payload_ref.is_some() {
            return Ok(());
        }
        let snapshot = self.decode_snapshot()?;
        self.validate_snapshot(&snapshot)
    }

    fn validate_metadata(&self) -> Result<(), RunCheckpointValidationError> {
        if self.schema_version == 1 {
            if self.payload_base64.is_none()
                || self.payload_ref.is_some()
                || self.payload_encoding != CheckpointPayloadEncoding::Identity
            {
                return Err(RunCheckpointValidationError::InvalidPayload);
            }
            return Ok(());
        }
        let has_inline = self.payload_base64.is_some();
        let has_ref = self.payload_ref.is_some();
        if has_inline == has_ref
            || self.payload_encoding != CheckpointPayloadEncoding::Zstd
            || !is_sha256(&self.stored_payload_digest)
            || self.uncompressed_size == 0
            || self.uncompressed_size > CHECKPOINT_MAX_UNCOMPRESSED_BYTES as u64
            || self.stored_size == 0
            || (has_inline && self.stored_size > INLINE_CHECKPOINT_MAX_BYTES as u64)
            || self.payload_ref.as_deref().is_some_and(|reference| {
                reference != format!("checkpoint://sha256/{}", self.stored_payload_digest)
            })
        {
            return Err(RunCheckpointValidationError::InvalidPayload);
        }
        Ok(())
    }

    fn validate_snapshot(
        &self,
        snapshot: &CheckpointSnapshot,
    ) -> Result<(), RunCheckpointValidationError> {
        if snapshot.tenant_id != self.tenant_id
            || snapshot.run_id != self.run_id
            || snapshot.session_id != self.session_id
            || snapshot.attempt_id != self.attempt_id
            || snapshot.sequence != self.sequence
            || snapshot.status != self.status
            || snapshot.digest != self.kernel_digest
            || !snapshot.verify_digest()
        {
            return Err(RunCheckpointValidationError::InvalidPayload);
        }
        Ok(())
    }

    pub fn decode_snapshot(&self) -> Result<CheckpointSnapshot, RunCheckpointValidationError> {
        let encoded = self
            .payload_base64
            .as_deref()
            .ok_or(RunCheckpointValidationError::InvalidPayload)?;
        let payload = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| RunCheckpointValidationError::InvalidPayload)?;
        self.decode_stored_payload(&payload)
    }

    pub fn decode_snapshot_with_payload(
        &self,
        stored: &[u8],
    ) -> Result<CheckpointSnapshot, RunCheckpointValidationError> {
        if self.payload_ref.is_none() || self.payload_base64.is_some() {
            return Err(RunCheckpointValidationError::InvalidPayload);
        }
        self.decode_stored_payload(stored)
    }

    fn decode_stored_payload(
        &self,
        stored: &[u8],
    ) -> Result<CheckpointSnapshot, RunCheckpointValidationError> {
        if stored.is_empty() {
            return Err(RunCheckpointValidationError::InvalidPayload);
        }
        let payload = match self.schema_version {
            1 => {
                if stored.len() > INLINE_CHECKPOINT_MAX_BYTES
                    || hex::encode(Sha256::digest(stored)) != self.payload_digest
                {
                    return Err(RunCheckpointValidationError::InvalidPayload);
                }
                stored.to_vec()
            }
            RUN_CHECKPOINT_SCHEMA_VERSION => {
                if stored.len() as u64 != self.stored_size
                    || hex::encode(Sha256::digest(stored)) != self.stored_payload_digest
                {
                    return Err(RunCheckpointValidationError::InvalidPayload);
                }
                let payload = zstd::bulk::decompress(stored, CHECKPOINT_MAX_UNCOMPRESSED_BYTES)
                    .map_err(|_| RunCheckpointValidationError::InvalidPayload)?;
                if payload.len() as u64 != self.uncompressed_size
                    || hex::encode(Sha256::digest(&payload)) != self.payload_digest
                {
                    return Err(RunCheckpointValidationError::InvalidPayload);
                }
                payload
            }
            version => {
                return Err(RunCheckpointValidationError::UnsupportedSchemaVersion(
                    version,
                ));
            }
        };
        let snapshot = serde_json::from_slice(&payload)
            .map_err(|_| RunCheckpointValidationError::InvalidPayload)?;
        self.validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentResultSource {
    pub tool_call_id: String,
    pub delegation_id: Uuid,
    pub binding_digest: String,
    pub child_run_id: Uuid,
    pub child_terminal_event_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentResultOutcome {
    pub terminal_status: RunStatus,
    pub content: Value,
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentBudgetUsage {
    pub tokens: u64,
    pub cost_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentResultDelivery {
    pub tool_call_id: String,
    pub delegation_id: Uuid,
    pub binding_digest: String,
    pub child_run_id: Uuid,
    pub child_terminal_event_id: Uuid,
    pub terminal_status: RunStatus,
    pub content: Value,
    pub is_error: bool,
    /// Actual child model usage, settled into the parent when the bound result
    /// is accepted. Older receipts deserialize as zero and keep their legacy
    /// digest valid.
    #[serde(default)]
    pub usage: SubagentBudgetUsage,
    /// Exact provider-neutral messages visible to the child model, excluding
    /// the current role instructions. Empty means a legacy receipt written
    /// before typed continuation history was available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<Message>,
    pub digest: String,
}

impl SubagentResultDelivery {
    #[must_use]
    pub fn new(source: SubagentResultSource, outcome: SubagentResultOutcome) -> Self {
        let mut result = Self {
            tool_call_id: source.tool_call_id,
            delegation_id: source.delegation_id,
            binding_digest: source.binding_digest,
            child_run_id: source.child_run_id,
            child_terminal_event_id: source.child_terminal_event_id,
            terminal_status: outcome.terminal_status,
            content: outcome.content,
            is_error: outcome.is_error,
            usage: SubagentBudgetUsage::default(),
            transcript: Vec::new(),
            digest: String::new(),
        };
        result.digest = result.calculate_digest();
        result
    }

    #[must_use]
    pub fn new_with_usage_and_transcript(
        source: SubagentResultSource,
        outcome: SubagentResultOutcome,
        usage: SubagentBudgetUsage,
        transcript: Vec<Message>,
    ) -> Self {
        let mut result = Self::new_with_usage(source, outcome, usage);
        result.transcript = transcript;
        result.digest = result.calculate_digest();
        result
    }

    #[must_use]
    pub fn new_with_usage(
        source: SubagentResultSource,
        outcome: SubagentResultOutcome,
        usage: SubagentBudgetUsage,
    ) -> Self {
        let mut result = Self::new(source, outcome);
        result.usage = usage;
        result.digest = result.calculate_digest();
        result
    }

    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.digest == self.calculate_digest()
    }

    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.validate()
    }

    fn calculate_digest(&self) -> String {
        let material = if !self.transcript.is_empty() {
            serde_json::to_vec(&(
                "agent-runtime-subagent-result-v3",
                &self.tool_call_id,
                self.delegation_id,
                &self.binding_digest,
                self.child_run_id,
                self.child_terminal_event_id,
                self.terminal_status,
                &self.content,
                self.is_error,
                self.usage,
                &self.transcript,
            ))
            .expect("typed subagent result digest material is serializable")
        } else if self.usage == SubagentBudgetUsage::default() {
            // Preserve verification of schema-less result receipts written by
            // Runtime versions before child usage became part of settlement.
            serde_json::to_vec(&(
                &self.tool_call_id,
                self.delegation_id,
                &self.binding_digest,
                self.child_run_id,
                self.child_terminal_event_id,
                self.terminal_status,
                &self.content,
                self.is_error,
            ))
            .expect("legacy subagent result digest material is serializable")
        } else {
            serde_json::to_vec(&(
                "agent-runtime-subagent-result-v2",
                &self.tool_call_id,
                self.delegation_id,
                &self.binding_digest,
                self.child_run_id,
                self.child_terminal_event_id,
                self.terminal_status,
                &self.content,
                self.is_error,
                self.usage,
            ))
            .expect("subagent result digest material is serializable")
        };
        hex::encode(Sha256::digest(material))
    }

    fn validate(&self) -> bool {
        !self.tool_call_id.trim().is_empty()
            && self.tool_call_id.len() <= 256
            && !self.delegation_id.is_nil()
            && is_sha256(&self.binding_digest)
            && !self.child_run_id.is_nil()
            && !self.child_terminal_event_id.is_nil()
            && self.terminal_status.is_terminal()
            && self.is_error == (self.terminal_status != RunStatus::Succeeded)
            && serde_json::to_vec(&self.content)
                .is_ok_and(|content| content.len() <= SUBAGENT_RESULT_MAX_BYTES)
            && (self.transcript.is_empty()
                || (valid_subagent_transcript(&self.transcript)
                    && serde_json::to_vec(&self.transcript)
                        .is_ok_and(|transcript| transcript.len() <= SUBAGENT_TRANSCRIPT_MAX_BYTES)
                    && (self.terminal_status != RunStatus::Succeeded
                        || self
                            .transcript
                            .last()
                            .is_some_and(|message| message.role == Role::Assistant))))
            && self.verify_digest()
    }
}

fn valid_subagent_transcript(transcript: &[Message]) -> bool {
    if transcript.is_empty() || transcript.first().map(|message| message.role) != Some(Role::User) {
        return false;
    }
    let mut calls = std::collections::BTreeSet::new();
    let mut results = std::collections::BTreeSet::new();
    for message in transcript {
        if message.content.is_empty() || message.role == Role::System {
            return false;
        }
        match message.role {
            Role::System => return false,
            Role::User => {
                if message.content.iter().any(|part| {
                    matches!(
                        part,
                        ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. }
                    )
                }) {
                    return false;
                }
            }
            Role::Assistant => {
                for part in &message.content {
                    match part {
                        ContentPart::ToolResult { .. } => return false,
                        ContentPart::ToolCall {
                            tool_call_id, name, ..
                        } if tool_call_id.trim().is_empty()
                            || tool_call_id.len() > 256
                            || name.trim().is_empty()
                            || name.len() > 256
                            || !calls.insert(tool_call_id.as_str()) =>
                        {
                            return false;
                        }
                        ContentPart::ToolCall { .. } => {}
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                let [
                    ContentPart::ToolResult {
                        tool_call_id,
                        content: _,
                    },
                ] = message.content.as_slice()
                else {
                    return false;
                };
                if tool_call_id.trim().is_empty()
                    || tool_call_id.len() > 256
                    || !calls.contains(tool_call_id.as_str())
                    || !results.insert(tool_call_id.as_str())
                {
                    return false;
                }
            }
        }
    }
    calls == results
}

fn valid_subagent_conversation_history(history: &[SubagentConversationTurn]) -> bool {
    if history.len() > SUBAGENT_HISTORY_MAX_TURNS
        || serde_json::to_vec(history)
            .map_or(true, |encoded| encoded.len() > SUBAGENT_HISTORY_MAX_BYTES)
    {
        return false;
    }
    let mut previous_activation = None;
    let mut message_sequences = std::collections::BTreeSet::new();
    history.iter().all(|turn| {
        let ordered = previous_activation
            .map(|previous| turn.activation_ordinal > previous)
            .unwrap_or(true);
        previous_activation = Some(turn.activation_ordinal);
        ordered
            && message_sequences.insert(turn.message_sequence)
            && !turn.child_run_id.is_nil()
            && !turn.input.trim().is_empty()
            && turn.input.len() <= 32_000
            && turn.result.child_run_id == turn.child_run_id
            && turn.result.delegation_id == turn.child_run_id
            && turn.result.validate()
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunRecoveryCommand {
    pub schema_version: u32,
    pub message_id: Uuid,
    pub execution: RunExecutionCommand,
    pub checkpoint: RunCheckpointPublished,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_result: Option<SubagentResultDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<RunSteeringCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunRecoveryValidationError {
    #[error("unsupported recovery schema version {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("recovery execution command is invalid: {0}")]
    InvalidExecution(String),
    #[error("recovery checkpoint is invalid: {0}")]
    InvalidCheckpoint(String),
    #[error("recovery identity or fencing does not advance the checkpoint")]
    InvalidRecoveryBinding,
    #[error("legacy recovery must not carry a subagent result")]
    LegacySubagentResult,
    #[error("recovery before schema v3 must not carry steering")]
    LegacySteering,
    #[error("subagent result is invalid or does not resume a suspended checkpoint")]
    InvalidSubagentResult,
    #[error("steering is invalid or does not resume a running checkpoint")]
    InvalidSteering,
    #[error("recovery cannot carry both a subagent result and steering")]
    ConflictingRecoveryActions,
}

impl RunRecoveryCommand {
    pub fn validate(&self) -> Result<(), RunRecoveryValidationError> {
        if !(1..=RUN_RECOVERY_SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(RunRecoveryValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.schema_version == 1 && self.subagent_result.is_some() {
            return Err(RunRecoveryValidationError::LegacySubagentResult);
        }
        if self.schema_version < 3 && self.steering.is_some() {
            return Err(RunRecoveryValidationError::LegacySteering);
        }
        if self.subagent_result.is_some() && self.steering.is_some() {
            return Err(RunRecoveryValidationError::ConflictingRecoveryActions);
        }
        self.execution
            .validate()
            .map_err(|error| RunRecoveryValidationError::InvalidExecution(error.to_string()))?;
        self.checkpoint
            .validate()
            .map_err(|error| RunRecoveryValidationError::InvalidCheckpoint(error.to_string()))?;
        if self.message_id.is_nil()
            || self.execution.tenant_id != self.checkpoint.tenant_id
            || self.execution.run_id != self.checkpoint.run_id
            || self.execution.session_id != self.checkpoint.session_id
            || self.execution.attempt_id == self.checkpoint.attempt_id
            || self.execution.owner_epoch <= self.checkpoint.owner_epoch
            || self.execution.fencing_token == self.checkpoint.fencing_token
        {
            return Err(RunRecoveryValidationError::InvalidRecoveryBinding);
        }
        if self.subagent_result.as_ref().is_some_and(|result| {
            self.checkpoint.status != RunStatus::Suspended || !result.validate()
        }) {
            return Err(RunRecoveryValidationError::InvalidSubagentResult);
        }
        if self.steering.as_ref().is_some_and(|steering| {
            self.checkpoint.status != RunStatus::Running
                || steering.validate().is_err()
                || steering.tenant_id != self.execution.tenant_id
                || steering.run_id != self.execution.run_id
                || steering.attempt_id != self.execution.attempt_id
                || steering.worker_id != self.execution.worker_id
                || steering.worker_incarnation_id != self.execution.worker_incarnation_id
        }) {
            return Err(RunRecoveryValidationError::InvalidSteering);
        }
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub event_id: Uuid,
    pub schema_version: u32,
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub run_id: Uuid,
    pub sequence: u64,
    pub attempt_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub trace_id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
    pub digest: String,
}

impl EventEnvelope {
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        session_id: Uuid,
        run_id: Uuid,
        sequence: u64,
        attempt_id: Uuid,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Self {
        let digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&payload).expect("JSON value serialization is infallible"),
        ));
        Self {
            event_id: Uuid::now_v7(),
            schema_version: 1,
            tenant_id,
            session_id,
            run_id,
            sequence,
            attempt_id,
            timestamp: Utc::now(),
            trace_id: Uuid::now_v7(),
            event_type: event_type.into(),
            payload,
            digest,
        }
    }
}
