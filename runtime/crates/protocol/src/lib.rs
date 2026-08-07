use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use uuid::Uuid;

pub const RUN_QUEUED_SCHEMA_VERSION: u32 = 1;
pub const RUN_EXECUTION_SCHEMA_VERSION: u32 = 7;
pub const RUN_CANCELLATION_SCHEMA_VERSION: u32 = 2;
pub const RUN_STEERING_SCHEMA_VERSION: u32 = 1;
pub const RUN_STEERING_OUTCOME_SCHEMA_VERSION: u32 = 1;
pub const TOOL_APPROVAL_DECISION_SCHEMA_VERSION: u32 = 2;
pub const WORKER_HEARTBEAT_SCHEMA_VERSION: u32 = 2;
pub const RUN_EXECUTION_ACCEPTED_SCHEMA_VERSION: u32 = 2;

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
    pub input: String,
    pub budget: RunBudget,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubagentSpawnRequest {
    pub tool_call_id: String,
    pub delegation_id: Uuid,
    pub role: String,
    pub input: String,
    pub budget: RunBudget,
    pub binding_digest: String,
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
    pub fn artifact_digest_matches(&self, tenant_id: Uuid) -> bool {
        is_sha256(&self.artifact_digest)
            && hex::encode(Sha256::digest(self.canonical_bytes(tenant_id))) == self.artifact_digest
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
                .all(|name| portable_identifier(name, 120))
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
}

impl RunExecutionCommand {
    pub fn validate(&self) -> Result<(), RunExecutionValidationError> {
        if !(1..=RUN_EXECUTION_SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(RunExecutionValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
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
        if !self.budget.is_positive_and_finite() {
            return Err(RunExecutionValidationError::InvalidBudget);
        }
        Ok(())
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
            digest: String::new(),
        };
        result.digest = result.calculate_digest();
        result
    }

    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.digest == self.calculate_digest()
    }

    fn calculate_digest(&self) -> String {
        let material = serde_json::to_vec(&(
            &self.tool_call_id,
            self.delegation_id,
            &self.binding_digest,
            self.child_run_id,
            self.child_terminal_event_id,
            self.terminal_status,
            &self.content,
            self.is_error,
        ))
        .expect("subagent result digest material is serializable");
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
            && self.verify_digest()
    }
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
