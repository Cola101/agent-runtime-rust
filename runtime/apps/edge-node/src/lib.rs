use agent_protocol::{EdgeTaskClaims, EdgeTaskValidationError, RunStatus};
use agent_runtime_host::embedded::EmbeddedRuntime;
use agent_runtime_host::{LocalEvent, LocalRunState};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod daemon;
mod enrollment;
pub mod transport;
pub mod wire {
    tonic::include_proto!("agent.edge.v1");
}
pub use enrollment::{
    EdgeCapabilityManifest, EdgeDeviceIdentity, EdgeEnrollmentGrantClaims, EdgeSessionProofClaims,
    VerifiedEdgeEnrollment, VerifiedEdgeEnrollmentRequest, verify_edge_enrollment_grant,
    verify_edge_enrollment_request, verify_edge_session_proof,
};

const EDGE_TASK_TOKEN_VERSION: &str = "edge-task-v1";
const EDGE_OUTBOX_ACK_TOKEN_VERSION: &str = "edge-outbox-ack-v1";
const EDGE_ENROLLMENT_REVOCATION_TOKEN_VERSION: &str = "edge-enrollment-revocation-v1";
const MAX_CONTROL_PLANE_KEYS: usize = 16;
const MAX_TOKEN_BYTES: usize = 64 * 1024;
const MAX_OUTBOX_ACK_LIFETIME_MS: i64 = 5 * 60 * 1_000;
const MAX_REVOCATION_TOKEN_LIFETIME_MS: i64 = 5 * 60 * 1_000;
const MAX_RUNTIME_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

type WorkspaceIdentity = (Uuid, Uuid, Uuid);
type WorkspaceExecutionLock = std::sync::Arc<tokio::sync::Mutex<()>>;

pub struct EdgeControlPlaneTrust {
    keys: BTreeMap<String, VerifyingKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedEdgeTask {
    pub claims: EdgeTaskClaims,
    pub signing_key_id: String,
    pub task_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeTaskReceiptStatus {
    Accepted,
    WaitingApproval,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Indeterminate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeTaskReceipt {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub task_digest: String,
    pub enrollment_id: Uuid,
    pub capability_manifest_digest: String,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub invocation: agent_protocol::RuntimeInvocationContext,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_owner_epoch: u64,
    pub status: EdgeTaskReceiptStatus,
    pub output: String,
    pub last_runtime_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeRuntimeEvent {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub task_digest: String,
    pub enrollment_id: Uuid,
    pub capability_manifest_digest: String,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub invocation: agent_protocol::RuntimeInvocationContext,
    pub workspace_owner_epoch: u64,
    pub event_id: Uuid,
    pub session_id: Uuid,
    pub run_id: Uuid,
    pub sequence: u64,
    pub attempt_id: Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub trace_id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: serde_json::Value,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EdgeOutboxPayload {
    TaskReceipt(EdgeTaskReceipt),
    RuntimeEvent(EdgeRuntimeEvent),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeOutboxRecord {
    pub sequence: u64,
    pub payload: EdgeOutboxPayload,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeOutboxAckClaims {
    pub schema_version: u32,
    pub ack_id: Uuid,
    pub session_id: Uuid,
    pub enrollment_id: Uuid,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub through_sequence: u64,
    pub batch_digest: String,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeEnrollmentRevocationClaims {
    pub schema_version: u32,
    pub revocation_id: Uuid,
    pub enrollment_id: Uuid,
    pub device_id: Uuid,
    pub node_id: Uuid,
    pub node_generation: u64,
    pub reason_code: String,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Debug)]
pub struct EdgeTaskReservation {
    is_new: bool,
    receipt: EdgeTaskReceipt,
}

impl EdgeTaskReservation {
    #[must_use]
    pub const fn is_new(&self) -> bool {
        self.is_new
    }

    #[must_use]
    pub const fn receipt(&self) -> &EdgeTaskReceipt {
        &self.receipt
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EdgeNodeState {
    schema_version: u32,
    #[serde(default)]
    device_id: Uuid,
    #[serde(default)]
    device_public_key_base64url: String,
    #[serde(default)]
    enrollment_id: Uuid,
    #[serde(default)]
    capability_manifest_digest: String,
    #[serde(default)]
    enrollment_grant_digest: String,
    #[serde(default)]
    revoked_enrollment_id: Uuid,
    #[serde(default)]
    enrollment_revocation_digest: String,
    #[serde(default)]
    revoked_at_unix_ms: i64,
    #[serde(default)]
    node_id: Uuid,
    #[serde(default)]
    node_generation: u64,
    #[serde(default)]
    workspace_owner_epochs: BTreeMap<String, u64>,
    acked_outbox_sequence: u64,
    next_outbox_sequence: u64,
    receipts: BTreeMap<Uuid, EdgeTaskReceipt>,
    outbox: Vec<EdgeOutboxRecord>,
}

impl Default for EdgeNodeState {
    fn default() -> Self {
        Self {
            schema_version: 3,
            device_id: Uuid::nil(),
            device_public_key_base64url: String::new(),
            enrollment_id: Uuid::nil(),
            capability_manifest_digest: String::new(),
            enrollment_grant_digest: String::new(),
            revoked_enrollment_id: Uuid::nil(),
            enrollment_revocation_digest: String::new(),
            revoked_at_unix_ms: 0,
            node_id: Uuid::nil(),
            node_generation: 0,
            workspace_owner_epochs: BTreeMap::new(),
            acked_outbox_sequence: 0,
            next_outbox_sequence: 1,
            receipts: BTreeMap::new(),
            outbox: Vec::new(),
        }
    }
}

pub struct EdgeNodeStore {
    path: PathBuf,
    state: std::sync::Mutex<EdgeNodeState>,
    _writer_lock: std::fs::File,
}

pub struct EdgeNode {
    enrollment: VerifiedEdgeEnrollment,
    trust: EdgeControlPlaneTrust,
    store: EdgeNodeStore,
    runtime: EmbeddedRuntime,
    workspace_locks: std::sync::Mutex<BTreeMap<WorkspaceIdentity, WorkspaceExecutionLock>>,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum EdgeNodeError {
    #[error("edge control-plane trust set is invalid")]
    InvalidTrustSet,
    #[error("edge task token format is invalid")]
    InvalidTaskToken,
    #[error("edge task signing key is not trusted")]
    UnknownSigningKey,
    #[error("edge task signature is invalid")]
    InvalidTaskSignature,
    #[error(transparent)]
    InvalidTaskClaims(#[from] EdgeTaskValidationError),
    #[error("edge task targets another node")]
    WrongNode,
    #[error("edge task targets another node generation")]
    WrongNodeGeneration,
    #[error("edge task targets another Enrollment")]
    WrongEnrollment,
    #[error("edge task requires a capability outside the approved node surface")]
    UnapprovedCapability,
    #[error("edge node durable state is invalid: {0}")]
    InvalidState(String),
    #[error("edge task id is already bound to another signed payload")]
    TaskIdentityConflict,
    #[error("edge task Workspace owner epoch is stale")]
    StaleWorkspaceOwnerEpoch,
    #[error("edge task receipt is invalid: {0}")]
    InvalidReceipt(String),
    #[error("edge outbox cursor is invalid")]
    InvalidOutboxCursor,
    #[error("edge outbox ACK authority is invalid")]
    InvalidOutboxAck,
    #[error("edge node durable state operation failed: {0}")]
    StateIo(String),
    #[error("edge node identity is invalid")]
    InvalidNodeIdentity,
    #[error("edge capability manifest is invalid")]
    InvalidCapabilityManifest,
    #[error("edge Enrollment request is invalid")]
    InvalidEnrollmentRequest,
    #[error("edge Enrollment grant is invalid")]
    InvalidEnrollmentGrant,
    #[error("edge Enrollment device binding does not match")]
    EnrollmentDeviceMismatch,
    #[error("edge Enrollment capability binding does not match")]
    EnrollmentCapabilityMismatch,
    #[error("edge live session device proof is invalid")]
    InvalidSessionProof,
    #[error("edge Enrollment revocation authority is invalid")]
    InvalidEnrollmentRevocation,
    #[error("edge Enrollment is revoked")]
    EnrollmentRevoked,
    #[error("edge Enrollment has expired")]
    EnrollmentExpired,
    #[error("edge outbound connection configuration is invalid")]
    InvalidOutboundConfiguration,
    #[error("edge outbound connection configuration is invalid: {0}")]
    InvalidOutboundConfigurationWithDetail(String),
    #[error("edge outbound transport failed: {0}")]
    Transport(String),
    #[error("edge control-plane stream frame is invalid")]
    InvalidControlFrame,
    #[error("edge Runtime execution failed: {0}")]
    Runtime(String),
}

impl EdgeControlPlaneTrust {
    pub fn new(keys: BTreeMap<String, VerifyingKey>) -> Result<Self, EdgeNodeError> {
        if keys.is_empty()
            || keys.len() > MAX_CONTROL_PLANE_KEYS
            || keys.keys().any(|key_id| !valid_key_id(key_id))
        {
            return Err(EdgeNodeError::InvalidTrustSet);
        }
        Ok(Self { keys })
    }

    fn verifying_key(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(key_id)
    }
}

fn valid_key_id(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.len() <= 64
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn verify_edge_task_token(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    expected_node_id: Uuid,
    expected_node_generation: u64,
    now_unix_ms: i64,
) -> Result<VerifiedEdgeTask, EdgeNodeError> {
    verify_edge_task_token_at(
        token,
        trust,
        expected_node_id,
        expected_node_generation,
        Some(now_unix_ms),
    )
}

pub fn verify_edge_task_token_for_enrollment(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    enrollment: &VerifiedEdgeEnrollment,
    now_unix_ms: i64,
) -> Result<VerifiedEdgeTask, EdgeNodeError> {
    let task = verify_edge_task_token(
        token,
        trust,
        enrollment.claims.node_id,
        enrollment.claims.node_generation,
        now_unix_ms,
    )?;
    validate_task_enrollment(&task, enrollment)?;
    validate_enrollment_at(enrollment, now_unix_ms)?;
    Ok(task)
}

fn validate_task_enrollment(
    task: &VerifiedEdgeTask,
    enrollment: &VerifiedEdgeEnrollment,
) -> Result<(), EdgeNodeError> {
    if task.claims.enrollment_id != enrollment.claims.enrollment_id
        || task.claims.capability_manifest_digest != enrollment.claims.capability_manifest_digest
    {
        return Err(EdgeNodeError::WrongEnrollment);
    }
    if !task
        .claims
        .required_capabilities
        .is_subset(&enrollment.claims.approved_capabilities)
    {
        return Err(EdgeNodeError::UnapprovedCapability);
    }
    Ok(())
}

fn validate_enrollment_at(
    enrollment: &VerifiedEdgeEnrollment,
    now_unix_ms: i64,
) -> Result<(), EdgeNodeError> {
    if enrollment.claims.issued_at_unix_ms > now_unix_ms
        || enrollment.claims.expires_at_unix_ms <= now_unix_ms
    {
        return Err(EdgeNodeError::EnrollmentExpired);
    }
    Ok(())
}

fn verify_edge_task_token_at(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    expected_node_id: Uuid,
    expected_node_generation: u64,
    validation_time: Option<i64>,
) -> Result<VerifiedEdgeTask, EdgeNodeError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(EdgeNodeError::InvalidTaskToken);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let key_id = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(EDGE_TASK_TOKEN_VERSION)
        || key_id.is_none_or(|value| !valid_key_id(value))
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidTaskToken);
    }
    let key_id = key_id.expect("validated above");
    let payload = payload.expect("validated above");
    let signature = URL_SAFE_NO_PAD
        .decode(signature.expect("validated above"))
        .map_err(|_| EdgeNodeError::InvalidTaskSignature)?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| EdgeNodeError::InvalidTaskSignature)?;
    let signed = format!("{EDGE_TASK_TOKEN_VERSION}.{key_id}.{payload}");
    trust
        .keys
        .get(key_id)
        .ok_or(EdgeNodeError::UnknownSigningKey)?
        .verify_strict(signed.as_bytes(), &signature)
        .map_err(|_| EdgeNodeError::InvalidTaskSignature)?;
    let claims = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| EdgeNodeError::InvalidTaskToken)?;
    let claims = serde_json::from_slice::<EdgeTaskClaims>(&claims)
        .map_err(|_| EdgeNodeError::InvalidTaskToken)?;
    claims.validate_at(validation_time.unwrap_or(claims.issued_at_unix_ms))?;
    if claims.node_id != expected_node_id {
        return Err(EdgeNodeError::WrongNode);
    }
    if claims.node_generation != expected_node_generation {
        return Err(EdgeNodeError::WrongNodeGeneration);
    }
    Ok(VerifiedEdgeTask {
        claims,
        signing_key_id: key_id.into(),
        task_digest: hex::encode(Sha256::digest(signed.as_bytes())),
    })
}

impl EdgeNodeStore {
    pub fn open_enrolled(
        state_root: impl AsRef<Path>,
        enrollment: &VerifiedEdgeEnrollment,
    ) -> Result<Self, EdgeNodeError> {
        let store = Self::load(state_root)?;
        store.activate_enrollment(enrollment)?;
        Ok(store)
    }

    fn load(state_root: impl AsRef<Path>) -> Result<Self, EdgeNodeError> {
        let state_root = state_root.as_ref();
        if let Ok(metadata) = std::fs::symlink_metadata(state_root)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(EdgeNodeError::InvalidState(
                "state root must be a real directory".into(),
            ));
        }
        std::fs::create_dir_all(state_root)
            .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(state_root, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
        }
        let writer_lock = acquire_writer_lock(state_root)?;
        let path = state_root.join("edge-node-state.json");
        let state = if path.exists() {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(EdgeNodeError::InvalidState(
                    "state file must be a regular file".into(),
                ));
            }
            let body =
                std::fs::read(&path).map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
            serde_json::from_slice::<EdgeNodeState>(&body)
                .map_err(|error| EdgeNodeError::InvalidState(error.to_string()))?
        } else {
            EdgeNodeState::default()
        };
        validate_node_state(&state)?;
        Ok(Self {
            path,
            state: std::sync::Mutex::new(state),
            _writer_lock: writer_lock,
        })
    }

    fn activate_enrollment(
        &self,
        enrollment: &VerifiedEdgeEnrollment,
    ) -> Result<(), EdgeNodeError> {
        let claims = &enrollment.claims;
        if claims.enrollment_id.is_nil()
            || claims.device_id.is_nil()
            || claims.device_public_key_base64url.is_empty()
            || claims.node_id.is_nil()
            || claims.node_generation == 0
            || !is_sha256(&claims.capability_manifest_digest)
            || !is_sha256(&enrollment.grant_digest)
            || claims.approved_capabilities.is_empty()
        {
            return Err(EdgeNodeError::InvalidNodeIdentity);
        }
        let mut current = self.lock_state()?;
        let is_unbound = state_is_unbound(&current);
        let exact = state_is_enrolled(&current)
            && current.device_id == claims.device_id
            && current.device_public_key_base64url == claims.device_public_key_base64url
            && current.enrollment_id == claims.enrollment_id
            && current.node_id == claims.node_id
            && current.node_generation == claims.node_generation
            && current.capability_manifest_digest == claims.capability_manifest_digest
            && current.enrollment_grant_digest == enrollment.grant_digest;
        if exact {
            if current.revoked_enrollment_id == claims.enrollment_id {
                return Err(EdgeNodeError::EnrollmentRevoked);
            }
            return Ok(());
        }
        let is_authorized_successor = state_is_enrolled(&current)
            && current.device_id == claims.device_id
            && current.device_public_key_base64url == claims.device_public_key_base64url
            && current.node_id == claims.node_id
            && claims.node_generation > current.node_generation
            && current
                .receipts
                .values()
                .all(|receipt| receipt_is_terminal(&receipt.status));
        if !is_unbound && !is_authorized_successor {
            return Err(EdgeNodeError::InvalidNodeIdentity);
        }
        let mut next = current.clone();
        next.device_id = claims.device_id;
        next.device_public_key_base64url = claims.device_public_key_base64url.clone();
        next.enrollment_id = claims.enrollment_id;
        next.node_id = claims.node_id;
        next.node_generation = claims.node_generation;
        next.capability_manifest_digest = claims.capability_manifest_digest.clone();
        next.enrollment_grant_digest = enrollment.grant_digest.clone();
        next.revoked_enrollment_id = Uuid::nil();
        next.enrollment_revocation_digest.clear();
        next.revoked_at_unix_ms = 0;
        persist_node_state(&self.path, &next)?;
        *current = next;
        Ok(())
    }

    pub fn reserve(&self, task: &VerifiedEdgeTask) -> Result<EdgeTaskReservation, EdgeNodeError> {
        let mut current = self.lock_state()?;
        if let Some(receipt) = current.receipts.get(&task.claims.task_id) {
            if receipt.task_digest != task.task_digest || receipt.run_id != task.claims.run_id {
                return Err(EdgeNodeError::TaskIdentityConflict);
            }
            return Ok(EdgeTaskReservation {
                is_new: false,
                receipt: receipt.clone(),
            });
        }
        if current
            .receipts
            .values()
            .any(|receipt| receipt.run_id == task.claims.run_id)
        {
            return Err(EdgeNodeError::TaskIdentityConflict);
        }
        let workspace_key = workspace_epoch_key(task.claims.invocation);
        if current
            .workspace_owner_epochs
            .get(&workspace_key)
            .is_some_and(|highest| task.claims.workspace_owner_epoch < *highest)
        {
            return Err(EdgeNodeError::StaleWorkspaceOwnerEpoch);
        }
        if current.receipts.len() >= 10_000 {
            return Err(EdgeNodeError::InvalidState(
                "task receipt capacity is exhausted".into(),
            ));
        }
        let receipt = EdgeTaskReceipt {
            schema_version: 1,
            task_id: task.claims.task_id,
            task_digest: task.task_digest.clone(),
            enrollment_id: task.claims.enrollment_id,
            capability_manifest_digest: task.claims.capability_manifest_digest.clone(),
            node_id: task.claims.node_id,
            node_generation: task.claims.node_generation,
            invocation: task.claims.invocation,
            run_id: task.claims.run_id,
            session_id: task.claims.session_id,
            workspace_owner_epoch: task.claims.workspace_owner_epoch,
            status: EdgeTaskReceiptStatus::Accepted,
            output: String::new(),
            last_runtime_sequence: 0,
        };
        let mut next = current.clone();
        next.workspace_owner_epochs
            .entry(workspace_key)
            .and_modify(|highest| {
                *highest = (*highest).max(task.claims.workspace_owner_epoch);
            })
            .or_insert(task.claims.workspace_owner_epoch);
        next.receipts.insert(receipt.task_id, receipt.clone());
        append_outbox(&mut next, EdgeOutboxPayload::TaskReceipt(receipt.clone()))?;
        persist_node_state(&self.path, &next)?;
        *current = next;
        Ok(EdgeTaskReservation {
            is_new: true,
            receipt,
        })
    }

    fn lookup(&self, task: &VerifiedEdgeTask) -> Result<Option<EdgeTaskReceipt>, EdgeNodeError> {
        let current = self.lock_state()?;
        let Some(receipt) = current.receipts.get(&task.claims.task_id) else {
            return Ok(None);
        };
        if receipt.task_digest != task.task_digest || receipt.run_id != task.claims.run_id {
            return Err(EdgeNodeError::TaskIdentityConflict);
        }
        if !receipt_is_terminal(&receipt.status)
            && current
                .workspace_owner_epochs
                .get(&workspace_epoch_key(receipt.invocation))
                .is_some_and(|highest| receipt.workspace_owner_epoch < *highest)
        {
            return Err(EdgeNodeError::StaleWorkspaceOwnerEpoch);
        }
        Ok(Some(receipt.clone()))
    }

    pub fn complete(&self, receipt: EdgeTaskReceipt) -> Result<(), EdgeNodeError> {
        self.complete_with_events(receipt, &[])
    }

    pub fn complete_with_events(
        &self,
        receipt: EdgeTaskReceipt,
        events: &[EdgeRuntimeEvent],
    ) -> Result<(), EdgeNodeError> {
        validate_receipt(&receipt)?;
        if receipt.status == EdgeTaskReceiptStatus::Accepted {
            return Err(EdgeNodeError::InvalidReceipt(
                "completion cannot remain accepted".into(),
            ));
        }
        let mut current = self.lock_state()?;
        let previous = current
            .receipts
            .get(&receipt.task_id)
            .ok_or_else(|| EdgeNodeError::InvalidReceipt("task was not reserved".into()))?;
        if previous.task_digest != receipt.task_digest
            || previous.enrollment_id != receipt.enrollment_id
            || previous.capability_manifest_digest != receipt.capability_manifest_digest
            || previous.node_id != receipt.node_id
            || previous.node_generation != receipt.node_generation
            || previous.invocation != receipt.invocation
            || previous.run_id != receipt.run_id
            || previous.session_id != receipt.session_id
            || previous.workspace_owner_epoch != receipt.workspace_owner_epoch
        {
            return Err(EdgeNodeError::TaskIdentityConflict);
        }
        if receipt_is_terminal(&previous.status) {
            return if previous == &receipt {
                Ok(())
            } else {
                Err(EdgeNodeError::InvalidReceipt(
                    "terminal receipt cannot be replaced".into(),
                ))
            };
        }
        let mut previous_sequence = previous.last_runtime_sequence;
        for event in events {
            validate_runtime_event(event)?;
            if event.task_id != receipt.task_id
                || event.task_digest != receipt.task_digest
                || event.enrollment_id != receipt.enrollment_id
                || event.capability_manifest_digest != receipt.capability_manifest_digest
                || event.node_id != receipt.node_id
                || event.node_generation != receipt.node_generation
                || event.invocation != receipt.invocation
                || event.workspace_owner_epoch != receipt.workspace_owner_epoch
                || event.run_id != receipt.run_id
                || event.session_id != receipt.session_id
                || event.sequence
                    != previous_sequence.checked_add(1).ok_or_else(|| {
                        EdgeNodeError::InvalidReceipt("Runtime event sequence is exhausted".into())
                    })?
            {
                return Err(EdgeNodeError::InvalidReceipt(
                    "runtime event does not continue the reserved task".into(),
                ));
            }
            previous_sequence = event.sequence;
        }
        if receipt.last_runtime_sequence != previous_sequence {
            return Err(EdgeNodeError::InvalidReceipt(
                "receipt cursor does not match its runtime events".into(),
            ));
        }
        let mut next = current.clone();
        next.receipts.insert(receipt.task_id, receipt.clone());
        for event in events {
            append_outbox(&mut next, EdgeOutboxPayload::RuntimeEvent(event.clone()))?;
        }
        append_outbox(&mut next, EdgeOutboxPayload::TaskReceipt(receipt))?;
        persist_node_state(&self.path, &next)?;
        *current = next;
        Ok(())
    }

    pub fn pending_outbox(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EdgeOutboxRecord>, EdgeNodeError> {
        if limit == 0 || limit > 256 {
            return Err(EdgeNodeError::InvalidOutboxCursor);
        }
        let state = self.lock_state()?;
        let cursor = after_sequence.max(state.acked_outbox_sequence);
        Ok(state
            .outbox
            .iter()
            .filter(|record| record.sequence > cursor)
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn apply_signed_outbox_ack(
        &self,
        token: &str,
        trust: &EdgeControlPlaneTrust,
        expected_session_id: Uuid,
        expected_batch_digest: &str,
        now_unix_ms: i64,
    ) -> Result<(), EdgeNodeError> {
        let claims = verify_outbox_ack_token(token, trust, now_unix_ms)?;
        let current = self.lock_state()?;
        if claims.session_id != expected_session_id
            || claims.enrollment_id != current.enrollment_id
            || claims.node_id != current.node_id
            || claims.node_generation != current.node_generation
            || claims.batch_digest != expected_batch_digest
        {
            return Err(EdgeNodeError::InvalidOutboxAck);
        }
        drop(current);
        self.ack_outbox(claims.through_sequence)
    }

    pub fn apply_signed_enrollment_revocation(
        &self,
        token: &str,
        trust: &EdgeControlPlaneTrust,
        now_unix_ms: i64,
    ) -> Result<(), EdgeNodeError> {
        let (claims, revocation_digest) =
            verify_enrollment_revocation_token(token, trust, now_unix_ms)?;
        let mut current = self.lock_state()?;
        if claims.enrollment_id != current.enrollment_id
            || claims.device_id != current.device_id
            || claims.node_id != current.node_id
            || claims.node_generation != current.node_generation
        {
            return Err(EdgeNodeError::InvalidEnrollmentRevocation);
        }
        if !current.revoked_enrollment_id.is_nil() {
            return if current.revoked_enrollment_id == claims.enrollment_id
                && current.enrollment_revocation_digest == revocation_digest
            {
                Ok(())
            } else {
                Err(EdgeNodeError::InvalidEnrollmentRevocation)
            };
        }
        let mut next = current.clone();
        next.revoked_enrollment_id = claims.enrollment_id;
        next.enrollment_revocation_digest = revocation_digest;
        next.revoked_at_unix_ms = claims.issued_at_unix_ms;
        persist_node_state(&self.path, &next)?;
        *current = next;
        Ok(())
    }

    fn ack_outbox(&self, sequence: u64) -> Result<(), EdgeNodeError> {
        let mut current = self.lock_state()?;
        let last_emitted = current.next_outbox_sequence.saturating_sub(1);
        if sequence < current.acked_outbox_sequence || sequence > last_emitted {
            return Err(EdgeNodeError::InvalidOutboxCursor);
        }
        if sequence == current.acked_outbox_sequence {
            return Ok(());
        }
        let mut next = current.clone();
        next.acked_outbox_sequence = sequence;
        next.outbox.retain(|record| record.sequence > sequence);
        persist_node_state(&self.path, &next)?;
        *current = next;
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, EdgeNodeState>, EdgeNodeError> {
        self.state
            .lock()
            .map_err(|_| EdgeNodeError::InvalidState("state lock is poisoned".into()))
    }
}

fn verify_outbox_ack_token(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    now_unix_ms: i64,
) -> Result<EdgeOutboxAckClaims, EdgeNodeError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(EdgeNodeError::InvalidOutboxAck);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let key_id = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(EDGE_OUTBOX_ACK_TOKEN_VERSION)
        || key_id.is_none_or(|value| !valid_key_id(value))
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidOutboxAck);
    }
    let key_id = key_id.expect("validated above");
    let payload = payload.expect("validated above");
    let signature = URL_SAFE_NO_PAD
        .decode(signature.expect("validated above"))
        .map_err(|_| EdgeNodeError::InvalidOutboxAck)?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| EdgeNodeError::InvalidOutboxAck)?;
    let signed = format!("{EDGE_OUTBOX_ACK_TOKEN_VERSION}.{key_id}.{payload}");
    trust
        .verifying_key(key_id)
        .ok_or(EdgeNodeError::UnknownSigningKey)?
        .verify_strict(signed.as_bytes(), &signature)
        .map_err(|_| EdgeNodeError::InvalidOutboxAck)?;
    let claims = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| EdgeNodeError::InvalidOutboxAck)?;
    let claims = serde_json::from_slice::<EdgeOutboxAckClaims>(&claims)
        .map_err(|_| EdgeNodeError::InvalidOutboxAck)?;
    if claims.schema_version != 1
        || claims.ack_id.is_nil()
        || claims.session_id.is_nil()
        || claims.enrollment_id.is_nil()
        || claims.node_id.is_nil()
        || claims.node_generation == 0
        || claims.through_sequence == 0
        || !is_sha256(&claims.batch_digest)
        || claims.issued_at_unix_ms > now_unix_ms
        || claims.expires_at_unix_ms <= now_unix_ms
        || claims.expires_at_unix_ms <= claims.issued_at_unix_ms
        || claims
            .expires_at_unix_ms
            .checked_sub(claims.issued_at_unix_ms)
            .is_none_or(|lifetime| lifetime > MAX_OUTBOX_ACK_LIFETIME_MS)
    {
        return Err(EdgeNodeError::InvalidOutboxAck);
    }
    Ok(claims)
}

fn verify_enrollment_revocation_token(
    token: &str,
    trust: &EdgeControlPlaneTrust,
    now_unix_ms: i64,
) -> Result<(EdgeEnrollmentRevocationClaims, String), EdgeNodeError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(EdgeNodeError::InvalidEnrollmentRevocation);
    }
    let mut parts = token.split('.');
    let version = parts.next();
    let key_id = parts.next();
    let payload = parts.next();
    let signature = parts.next();
    if version != Some(EDGE_ENROLLMENT_REVOCATION_TOKEN_VERSION)
        || key_id.is_none_or(|value| !valid_key_id(value))
        || payload.is_none()
        || signature.is_none()
        || parts.next().is_some()
    {
        return Err(EdgeNodeError::InvalidEnrollmentRevocation);
    }
    let key_id = key_id.expect("validated above");
    let payload = payload.expect("validated above");
    let signature = URL_SAFE_NO_PAD
        .decode(signature.expect("validated above"))
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRevocation)?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRevocation)?;
    let signed = format!("{EDGE_ENROLLMENT_REVOCATION_TOKEN_VERSION}.{key_id}.{payload}");
    trust
        .verifying_key(key_id)
        .ok_or(EdgeNodeError::UnknownSigningKey)?
        .verify_strict(signed.as_bytes(), &signature)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRevocation)?;
    let claims = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRevocation)?;
    let claims = serde_json::from_slice::<EdgeEnrollmentRevocationClaims>(&claims)
        .map_err(|_| EdgeNodeError::InvalidEnrollmentRevocation)?;
    if claims.schema_version != 1
        || claims.revocation_id.is_nil()
        || claims.enrollment_id.is_nil()
        || claims.device_id.is_nil()
        || claims.node_id.is_nil()
        || claims.node_generation == 0
        || !valid_reason_code(&claims.reason_code)
        || claims.issued_at_unix_ms > now_unix_ms
        || claims.expires_at_unix_ms <= now_unix_ms
        || claims.expires_at_unix_ms <= claims.issued_at_unix_ms
        || claims
            .expires_at_unix_ms
            .checked_sub(claims.issued_at_unix_ms)
            .is_none_or(|lifetime| lifetime > MAX_REVOCATION_TOKEN_LIFETIME_MS)
    {
        return Err(EdgeNodeError::InvalidEnrollmentRevocation);
    }
    Ok((claims, hex::encode(Sha256::digest(signed.as_bytes()))))
}

fn valid_reason_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

impl EdgeNode {
    pub fn new(
        enrollment: VerifiedEdgeEnrollment,
        trust: EdgeControlPlaneTrust,
        store: EdgeNodeStore,
        runtime: EmbeddedRuntime,
    ) -> Result<Self, EdgeNodeError> {
        store.activate_enrollment(&enrollment)?;
        Ok(Self {
            enrollment,
            trust,
            store,
            runtime,
            workspace_locks: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    pub async fn execute_task_token(
        &self,
        token: &str,
        now_unix_ms: i64,
    ) -> Result<EdgeTaskReceipt, EdgeNodeError> {
        // Expiry authorizes starting new work, not reading an already bound
        // receipt. Verify signature and immutable claims first so a delayed
        // duplicate can converge from durable evidence without reopening model
        // or Tool egress after its execution authority expired.
        let task = verify_edge_task_token_at(
            token,
            &self.trust,
            self.enrollment.claims.node_id,
            self.enrollment.claims.node_generation,
            None,
        )?;
        validate_task_enrollment(&task, &self.enrollment)?;
        let workspace_lock = {
            let mut locks = self.workspace_locks.lock().map_err(|_| {
                EdgeNodeError::InvalidState("Workspace lock map is poisoned".into())
            })?;
            locks
                .entry((
                    task.claims.invocation.tenant_id,
                    task.claims.invocation.application_id,
                    task.claims.invocation.workspace_id,
                ))
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _workspace_guard = workspace_lock.lock().await;
        if let Some(receipt) = self.store.lookup(&task)? {
            if receipt.status != EdgeTaskReceiptStatus::Accepted {
                return Ok(receipt);
            }
            return self.reconcile_accepted_task(&task);
        }
        validate_enrollment_at(&self.enrollment, now_unix_ms)?;
        task.claims.validate_at(now_unix_ms)?;
        let reservation = self.store.reserve(&task)?;
        debug_assert!(reservation.is_new());

        let outcome = self
            .runtime
            .execute_at_epoch(
                task.claims.invocation,
                task.claims.run_id,
                &task.claims.input,
                task.claims.workspace_owner_epoch,
            )
            .await;
        let events = self
            .runtime
            .replay_events(task.claims.invocation, task.claims.run_id, 0)
            .map_err(|error| EdgeNodeError::Runtime(error.to_string()))?;
        let receipt = match outcome {
            Ok(outcome) => receipt_from_runtime(
                &task,
                map_run_status(outcome.status),
                outcome.output,
                &events,
            ),
            Err(error) => receipt_from_runtime(
                &task,
                terminal_status(&events).unwrap_or(EdgeTaskReceiptStatus::Indeterminate),
                format!(
                    "runtime diagnostic sha256:{}",
                    hex::encode(Sha256::digest(error.to_string().as_bytes()))
                ),
                &events,
            ),
        };
        let edge_events = edge_runtime_events(&task, &events)?;
        self.store
            .complete_with_events(receipt.clone(), &edge_events)?;
        Ok(receipt)
    }

    pub fn pending_outbox(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<EdgeOutboxRecord>, EdgeNodeError> {
        self.store.pending_outbox(after_sequence, limit)
    }

    pub fn apply_signed_outbox_ack(
        &self,
        token: &str,
        expected_session_id: Uuid,
        expected_batch_digest: &str,
        now_unix_ms: i64,
    ) -> Result<(), EdgeNodeError> {
        self.store.apply_signed_outbox_ack(
            token,
            &self.trust,
            expected_session_id,
            expected_batch_digest,
            now_unix_ms,
        )
    }

    pub fn apply_signed_enrollment_revocation(
        &self,
        token: &str,
        now_unix_ms: i64,
    ) -> Result<(), EdgeNodeError> {
        self.store
            .apply_signed_enrollment_revocation(token, &self.trust, now_unix_ms)
    }

    fn reconcile_accepted_task(
        &self,
        task: &VerifiedEdgeTask,
    ) -> Result<EdgeTaskReceipt, EdgeNodeError> {
        let events = self
            .runtime
            .replay_events(task.claims.invocation, task.claims.run_id, 0)
            .map_err(|error| EdgeNodeError::Runtime(error.to_string()))?;
        let status = if let Some(status) = terminal_status(&events) {
            status
        } else {
            let record = self
                .runtime
                .read_run_record(task.claims.invocation, task.claims.run_id)
                .map_err(|error| EdgeNodeError::Runtime(error.to_string()))?;
            match record {
                Some(record)
                    if record.run_id == task.claims.run_id
                        && record.tenant_id == task.claims.invocation.tenant_id
                        && record.application_id == task.claims.invocation.application_id
                        && record.workload_identity_id
                            == task.claims.invocation.workload_identity_id
                        && record.workspace_id == task.claims.invocation.workspace_id
                        && record.agent_version_id == task.claims.invocation.agent_version_id
                        && record.model_policy_id == task.claims.invocation.model_policy_id =>
                {
                    match record.state {
                        LocalRunState::AwaitingApproval { .. }
                        | LocalRunState::ApprovalDecided { .. } => {
                            EdgeTaskReceiptStatus::WaitingApproval
                        }
                        LocalRunState::AwaitingMcpInput { .. }
                        | LocalRunState::McpInputDecided { .. } => EdgeTaskReceiptStatus::Suspended,
                        LocalRunState::Cancelled { .. } => EdgeTaskReceiptStatus::Cancelled,
                        LocalRunState::Running
                        | LocalRunState::Cancelling { .. }
                        | LocalRunState::Finished { .. }
                        | LocalRunState::Interrupted { .. } => EdgeTaskReceiptStatus::Indeterminate,
                    }
                }
                Some(_) => {
                    return Err(EdgeNodeError::Runtime(
                        "durable Run identity does not match the signed edge task".into(),
                    ));
                }
                None => EdgeTaskReceiptStatus::Indeterminate,
            }
        };
        let output = events
            .iter()
            .filter(|event| event.event_type == "model.output.delta")
            .filter_map(|event| event.payload["text"].as_str())
            .collect::<String>();
        let receipt = receipt_from_runtime(task, status, output, &events);
        let edge_events = edge_runtime_events(task, &events)?;
        self.store
            .complete_with_events(receipt.clone(), &edge_events)?;
        Ok(receipt)
    }
}

fn edge_runtime_events(
    task: &VerifiedEdgeTask,
    events: &[LocalEvent],
) -> Result<Vec<EdgeRuntimeEvent>, EdgeNodeError> {
    let mut converted = Vec::with_capacity(events.len());
    let mut expected_sequence = 1_u64;
    for event in events {
        if event.schema_version != 1
            || event.tenant_id != task.claims.invocation.tenant_id
            || event.application_id != task.claims.invocation.application_id
            || event.workload_identity_id != task.claims.invocation.workload_identity_id
            || event.workspace_id != task.claims.invocation.workspace_id
            || event.agent_version_id != task.claims.invocation.agent_version_id
            || event.model_policy_id != task.claims.invocation.model_policy_id
            || event.session_id != task.claims.session_id
            || event.run_id != task.claims.run_id
            || event.sequence != expected_sequence
        {
            return Err(EdgeNodeError::Runtime(
                "durable Runtime event identity or sequence does not match the signed edge task"
                    .into(),
            ));
        }
        let edge_event = EdgeRuntimeEvent {
            schema_version: 1,
            task_id: task.claims.task_id,
            task_digest: task.task_digest.clone(),
            enrollment_id: task.claims.enrollment_id,
            capability_manifest_digest: task.claims.capability_manifest_digest.clone(),
            node_id: task.claims.node_id,
            node_generation: task.claims.node_generation,
            invocation: task.claims.invocation,
            workspace_owner_epoch: task.claims.workspace_owner_epoch,
            event_id: event.event_id,
            session_id: event.session_id,
            run_id: event.run_id,
            sequence: event.sequence,
            attempt_id: event.attempt_id,
            timestamp: event.timestamp,
            trace_id: event.trace_id,
            event_type: event.event_type.clone(),
            payload: event.payload.clone(),
            digest: event.digest.clone(),
        };
        validate_runtime_event(&edge_event)?;
        converted.push(edge_event);
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            EdgeNodeError::InvalidState("Runtime event sequence is exhausted".into())
        })?;
    }
    Ok(converted)
}

fn map_run_status(status: RunStatus) -> EdgeTaskReceiptStatus {
    match status {
        RunStatus::Queued | RunStatus::Running => EdgeTaskReceiptStatus::Indeterminate,
        RunStatus::WaitingApproval => EdgeTaskReceiptStatus::WaitingApproval,
        RunStatus::Suspended => EdgeTaskReceiptStatus::Suspended,
        RunStatus::Succeeded => EdgeTaskReceiptStatus::Succeeded,
        RunStatus::Failed => EdgeTaskReceiptStatus::Failed,
        RunStatus::Cancelled => EdgeTaskReceiptStatus::Cancelled,
        RunStatus::TimedOut => EdgeTaskReceiptStatus::TimedOut,
        RunStatus::Indeterminate => EdgeTaskReceiptStatus::Indeterminate,
    }
}

fn terminal_status(events: &[LocalEvent]) -> Option<EdgeTaskReceiptStatus> {
    events
        .iter()
        .rev()
        .find_map(|event| match event.event_type.as_str() {
            "run.succeeded" => Some(EdgeTaskReceiptStatus::Succeeded),
            "run.failed" => Some(EdgeTaskReceiptStatus::Failed),
            "run.cancelled" => Some(EdgeTaskReceiptStatus::Cancelled),
            "run.timed_out" => Some(EdgeTaskReceiptStatus::TimedOut),
            "run.indeterminate" => Some(EdgeTaskReceiptStatus::Indeterminate),
            _ => None,
        })
}

fn receipt_from_runtime(
    task: &VerifiedEdgeTask,
    status: EdgeTaskReceiptStatus,
    output: String,
    events: &[LocalEvent],
) -> EdgeTaskReceipt {
    let output = if output.len() <= 1024 * 1024 {
        output
    } else {
        format!(
            "output omitted; sha256:{}",
            hex::encode(Sha256::digest(output.as_bytes()))
        )
    };
    EdgeTaskReceipt {
        schema_version: 1,
        task_id: task.claims.task_id,
        task_digest: task.task_digest.clone(),
        enrollment_id: task.claims.enrollment_id,
        capability_manifest_digest: task.claims.capability_manifest_digest.clone(),
        node_id: task.claims.node_id,
        node_generation: task.claims.node_generation,
        invocation: task.claims.invocation,
        run_id: task.claims.run_id,
        session_id: task.claims.session_id,
        workspace_owner_epoch: task.claims.workspace_owner_epoch,
        status,
        output,
        last_runtime_sequence: events.iter().map(|event| event.sequence).max().unwrap_or(0),
    }
}

#[cfg(unix)]
fn acquire_writer_lock(state_root: &Path) -> Result<std::fs::File, EdgeNodeError> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = state_root.join("edge-node.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    // SAFETY: `file` owns a live descriptor for the duration of this call and
    // remains stored in `EdgeNodeStore`, so the advisory lock cannot outlive it.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(EdgeNodeError::InvalidState(
            "another Edge Node owns this state root".into(),
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_writer_lock(_state_root: &Path) -> Result<std::fs::File, EdgeNodeError> {
    Err(EdgeNodeError::InvalidState(
        "single-writer Edge Node storage is not implemented on this platform".into(),
    ))
}

fn receipt_is_terminal(status: &EdgeTaskReceiptStatus) -> bool {
    matches!(
        status,
        EdgeTaskReceiptStatus::Succeeded
            | EdgeTaskReceiptStatus::Failed
            | EdgeTaskReceiptStatus::Cancelled
            | EdgeTaskReceiptStatus::TimedOut
            | EdgeTaskReceiptStatus::Indeterminate
    )
}

fn validate_receipt(receipt: &EdgeTaskReceipt) -> Result<(), EdgeNodeError> {
    if receipt.schema_version != 1
        || receipt.task_id.is_nil()
        || receipt.enrollment_id.is_nil()
        || !is_sha256(&receipt.capability_manifest_digest)
        || receipt.node_id.is_nil()
        || receipt.node_generation == 0
        || receipt.invocation.validate().is_err()
        || receipt.run_id.is_nil()
        || receipt.session_id != receipt.run_id
        || receipt.workspace_owner_epoch == 0
        || !is_sha256(&receipt.task_digest)
        || receipt.output.len() > 1024 * 1024
    {
        return Err(EdgeNodeError::InvalidReceipt(
            "identity, schema, digest, or output bound is invalid".into(),
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn workspace_epoch_key(invocation: agent_protocol::RuntimeInvocationContext) -> String {
    format!(
        "{}/{}/{}",
        invocation.tenant_id, invocation.application_id, invocation.workspace_id
    )
}

fn valid_workspace_epoch_key(key: &str) -> bool {
    let mut parts = key.split('/');
    let valid = parts
        .by_ref()
        .take(3)
        .all(|part| Uuid::parse_str(part).is_ok());
    valid && parts.next().is_none() && key.matches('/').count() == 2
}

fn append_outbox(
    state: &mut EdgeNodeState,
    payload: EdgeOutboxPayload,
) -> Result<(), EdgeNodeError> {
    if state.outbox.len() >= 10_000 || state.next_outbox_sequence == u64::MAX {
        return Err(EdgeNodeError::InvalidState(
            "outbox capacity or sequence is exhausted".into(),
        ));
    }
    state.outbox.push(EdgeOutboxRecord {
        sequence: state.next_outbox_sequence,
        payload,
    });
    state.next_outbox_sequence += 1;
    Ok(())
}

fn state_is_unbound(state: &EdgeNodeState) -> bool {
    state.device_id.is_nil()
        && state.device_public_key_base64url.is_empty()
        && state.enrollment_id.is_nil()
        && state.capability_manifest_digest.is_empty()
        && state.enrollment_grant_digest.is_empty()
        && state.revoked_enrollment_id.is_nil()
        && state.enrollment_revocation_digest.is_empty()
        && state.revoked_at_unix_ms == 0
        && state.node_id.is_nil()
        && state.node_generation == 0
}

fn state_is_enrolled(state: &EdgeNodeState) -> bool {
    !state.device_id.is_nil()
        && !state.device_public_key_base64url.is_empty()
        && !state.enrollment_id.is_nil()
        && is_sha256(&state.capability_manifest_digest)
        && is_sha256(&state.enrollment_grant_digest)
        && !state.node_id.is_nil()
        && state.node_generation > 0
}

fn validate_node_state(state: &EdgeNodeState) -> Result<(), EdgeNodeError> {
    let identity_is_unbound = state_is_unbound(state);
    let identity_is_bound = state_is_enrolled(state);
    let revocation_is_empty = state.revoked_enrollment_id.is_nil()
        && state.enrollment_revocation_digest.is_empty()
        && state.revoked_at_unix_ms == 0;
    let revocation_is_bound = state.revoked_enrollment_id == state.enrollment_id
        && is_sha256(&state.enrollment_revocation_digest)
        && state.revoked_at_unix_ms > 0;
    if state.schema_version != 3
        || (!identity_is_unbound && !identity_is_bound)
        || (identity_is_bound && !revocation_is_empty && !revocation_is_bound)
        || (identity_is_unbound
            && (!state.workspace_owner_epochs.is_empty()
                || !state.receipts.is_empty()
                || !state.outbox.is_empty()
                || state.acked_outbox_sequence != 0
                || state.next_outbox_sequence != 1))
        || state.next_outbox_sequence == 0
        || state.acked_outbox_sequence >= state.next_outbox_sequence
        || state.receipts.len() > 10_000
        || state.workspace_owner_epochs.len() > 10_000
        || state.outbox.len() > 10_000
        || state
            .workspace_owner_epochs
            .iter()
            .any(|(key, epoch)| !valid_workspace_epoch_key(key) || *epoch == 0)
    {
        return Err(EdgeNodeError::InvalidState(
            "schema, cursor, or capacity is invalid".into(),
        ));
    }
    let mut run_ids = BTreeSet::new();
    for (task_id, receipt) in &state.receipts {
        validate_receipt(receipt)?;
        if task_id != &receipt.task_id
            || !run_ids.insert(receipt.run_id)
            || receipt.node_id != state.node_id
            || receipt.node_generation > state.node_generation
            || state
                .workspace_owner_epochs
                .get(&workspace_epoch_key(receipt.invocation))
                .is_none_or(|highest| receipt.workspace_owner_epoch > *highest)
        {
            return Err(EdgeNodeError::InvalidState(
                "receipt map key or Run identity is not unique".into(),
            ));
        }
    }
    let mut expected_sequence = state
        .acked_outbox_sequence
        .checked_add(1)
        .ok_or_else(|| EdgeNodeError::InvalidState("outbox sequence is exhausted".into()))?;
    for record in &state.outbox {
        match &record.payload {
            EdgeOutboxPayload::TaskReceipt(receipt) => {
                validate_receipt(receipt)?;
                if receipt.node_id != state.node_id
                    || receipt.node_generation > state.node_generation
                {
                    return Err(EdgeNodeError::InvalidState(
                        "outbox receipt belongs to another node generation".into(),
                    ));
                }
            }
            EdgeOutboxPayload::RuntimeEvent(event) => {
                validate_runtime_event(event)?;
                if event.node_id != state.node_id || event.node_generation > state.node_generation {
                    return Err(EdgeNodeError::InvalidState(
                        "outbox event belongs to another node generation".into(),
                    ));
                }
            }
        }
        if record.sequence != expected_sequence || record.sequence >= state.next_outbox_sequence {
            return Err(EdgeNodeError::InvalidState(
                "outbox sequence is not contiguous".into(),
            ));
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| EdgeNodeError::InvalidState("outbox sequence is exhausted".into()))?;
    }
    if expected_sequence != state.next_outbox_sequence {
        return Err(EdgeNodeError::InvalidState(
            "outbox snapshot has an unacknowledged gap".into(),
        ));
    }
    Ok(())
}

fn validate_runtime_event(event: &EdgeRuntimeEvent) -> Result<(), EdgeNodeError> {
    let encoded_payload = serde_json::to_vec(&event.payload)
        .map_err(|error| EdgeNodeError::InvalidState(error.to_string()))?;
    if event.schema_version != 1
        || event.task_id.is_nil()
        || !is_sha256(&event.task_digest)
        || event.enrollment_id.is_nil()
        || !is_sha256(&event.capability_manifest_digest)
        || event.node_id.is_nil()
        || event.node_generation == 0
        || event.invocation.validate().is_err()
        || event.workspace_owner_epoch == 0
        || event.event_id.is_nil()
        || event.session_id != event.run_id
        || event.sequence == 0
        || event.attempt_id.is_nil()
        || event.trace_id.is_nil()
        || event.event_type.trim().is_empty()
        || !is_sha256(&event.digest)
        || encoded_payload.len() > MAX_RUNTIME_EVENT_PAYLOAD_BYTES
        || event.digest != hex::encode(Sha256::digest(&encoded_payload))
    {
        return Err(EdgeNodeError::InvalidState(
            "runtime outbox event identity or digest is invalid".into(),
        ));
    }
    Ok(())
}

fn persist_node_state(path: &Path, state: &EdgeNodeState) -> Result<(), EdgeNodeError> {
    validate_node_state(state)?;
    let body = serde_json::to_vec(state)
        .map_err(|error| EdgeNodeError::InvalidState(error.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| EdgeNodeError::InvalidState("state path has no parent".into()))?;
    let staging = parent.join(format!(".edge-node-state-{}.partial", Uuid::now_v7()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging)
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    use std::io::Write as _;
    let write_result = file
        .write_all(&body)
        .and_then(|()| file.sync_all())
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()));
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    std::fs::rename(&staging, path).map_err(|error| {
        let _ = std::fs::remove_file(&staging);
        EdgeNodeError::StateIo(error.to_string())
    })?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| EdgeNodeError::StateIo(error.to_string()))?;
    Ok(())
}
