//! Stable, transport-neutral client port for embedding the Runtime.
//!
//! Desktop, CLI and Java adapters should depend on this minimal client port
//! instead of calling the much larger [`crate::embedded::EmbeddedRuntime`]
//! implementation API.  Authentication and profile construction remain the
//! adapter's responsibility; once a profile is selected, every transport gets
//! the same submit, control, cursor and watch semantics here.

use crate::admission::RuntimeAdmissionError;
use crate::embedded::{
    EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY, EmbeddedEventSubscription, EmbeddedRuntime,
    EmbeddedRuntimeError, RUNTIME_EVENT_CURSOR_MAX_EVENTS, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
    RuntimeControlCommand, RuntimeControlReceipt, RuntimeEventCursorErrorCode,
    RuntimeEventCursorPage, RuntimeEventCursorRequest, RuntimeEventCursorState,
    RuntimeEventStreamItem,
};
use crate::{LocalRuntimeError, LocalSessionHead, SessionStoragePolicy};
use agent_protocol::{RuntimeInvocationContext, SessionConversationTurn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

pub const RUNTIME_CLIENT_SCHEMA_VERSION: u32 = 1;
pub const RUNTIME_CLIENT_CONTRACT_VERSION: u32 = 1;
/// Equal to the `RunExecutionCommand` validation bound. Advertising a larger
/// edge limit would durably accept work the Kernel will later refuse.
pub const RUNTIME_CLIENT_MAX_INPUT_BYTES: usize = 32_000;
pub const RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES: usize = 64 * 1024;
pub const RUNTIME_CLIENT_MAX_REQUIRED_CAPABILITIES: usize = 32;
pub const RUNTIME_CLIENT_MAX_CAPABILITY_BYTES: usize = 128;
pub const RUNTIME_CLIENT_MAX_SESSION_LIST_SIZE: usize = 256;
pub const RUNTIME_CLIENT_MAX_SESSION_HISTORY_TURNS: usize = 128;

pub const RUNTIME_CAPABILITY_RUN_SUBMIT: &str = "run.submit.v1";
pub const RUNTIME_CAPABILITY_RUN_CONTROL: &str = "run.control.v1";
pub const RUNTIME_CAPABILITY_EVENTS_CURSOR: &str = "events.cursor.v1";
pub const RUNTIME_CAPABILITY_EVENTS_WATCH: &str = "events.watch.v1";
pub const RUNTIME_CAPABILITY_RECOVERY_STARTUP: &str = "recovery.startup.v1";
pub const RUNTIME_CAPABILITY_SESSION_START: &str = "session.start.v1";
pub const RUNTIME_CAPABILITY_SESSION_CONTINUE: &str = "session.continue.v1";
pub const RUNTIME_CAPABILITY_SESSION_FORK: &str = "session.fork.v1";
pub const RUNTIME_CAPABILITY_SESSION_ROLLBACK: &str = "session.rollback.v1";
pub const RUNTIME_CAPABILITY_SESSION_READ: &str = "session.read.v1";
pub const RUNTIME_CAPABILITY_SESSION_LIST: &str = "session.list.v1";
pub const RUNTIME_CAPABILITY_SESSION_HISTORY: &str = "session.history.v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientHello {
    pub schema_version: u32,
    pub min_contract_version: u32,
    pub max_contract_version: u32,
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientDescriptor {
    pub schema_version: u32,
    pub contract_version: u32,
    pub runtime_version: String,
    pub capabilities: BTreeSet<String>,
    pub max_input_bytes: u64,
    pub max_action_json_bytes: u64,
    pub max_event_page_size: u32,
    pub max_event_stream_capacity: u32,
    pub max_session_list_size: u32,
    pub max_session_history_turns: u32,
    /// Session storage ceilings, published here so a caller can plan against
    /// them instead of meeting them halfway through a conversation.
    pub max_sessions_per_workspace: u32,
    pub max_sessions_per_tenant: u32,
    pub max_branches_per_session: u32,
    pub max_archived_generations_per_branch: u32,
    pub max_session_record_bytes: u64,
    pub max_turn_reserve_bytes: u64,
}

impl RuntimeClientDescriptor {
    #[must_use]
    pub fn current() -> Self {
        let storage = SessionStoragePolicy::default();
        Self {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: BTreeSet::from([
                RUNTIME_CAPABILITY_RUN_SUBMIT.into(),
                RUNTIME_CAPABILITY_RUN_CONTROL.into(),
                RUNTIME_CAPABILITY_EVENTS_CURSOR.into(),
                RUNTIME_CAPABILITY_EVENTS_WATCH.into(),
                RUNTIME_CAPABILITY_RECOVERY_STARTUP.into(),
                RUNTIME_CAPABILITY_SESSION_START.into(),
                RUNTIME_CAPABILITY_SESSION_CONTINUE.into(),
                RUNTIME_CAPABILITY_SESSION_FORK.into(),
                RUNTIME_CAPABILITY_SESSION_ROLLBACK.into(),
                RUNTIME_CAPABILITY_SESSION_READ.into(),
                RUNTIME_CAPABILITY_SESSION_LIST.into(),
                RUNTIME_CAPABILITY_SESSION_HISTORY.into(),
            ]),
            max_input_bytes: RUNTIME_CLIENT_MAX_INPUT_BYTES as u64,
            max_action_json_bytes: RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES as u64,
            max_event_page_size: RUNTIME_EVENT_CURSOR_MAX_EVENTS as u32,
            max_event_stream_capacity: EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY as u32,
            max_session_list_size: RUNTIME_CLIENT_MAX_SESSION_LIST_SIZE as u32,
            max_session_history_turns: RUNTIME_CLIENT_MAX_SESSION_HISTORY_TURNS as u32,
            max_sessions_per_workspace: storage.max_sessions_per_workspace as u32,
            max_sessions_per_tenant: storage.max_sessions_per_tenant as u32,
            max_branches_per_session: storage.max_branches_per_session as u32,
            max_archived_generations_per_branch: storage.max_archived_generations_per_branch as u32,
            max_session_record_bytes: storage.max_session_record_bytes as u64,
            max_turn_reserve_bytes: storage.max_turn_reserve_bytes as u64,
        }
    }

    fn negotiate(hello: &RuntimeClientHello) -> Result<Self, RuntimeClientError> {
        if hello.schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || hello.min_contract_version == 0
            || hello.max_contract_version < hello.min_contract_version
            || hello.required_capabilities.len() > RUNTIME_CLIENT_MAX_REQUIRED_CAPABILITIES
            || hello.required_capabilities.iter().any(|capability| {
                capability.is_empty()
                    || capability.len() > RUNTIME_CLIENT_MAX_CAPABILITY_BYTES
                    || !capability.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'-' | b'_')
                    })
            })
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime client initialization",
            ));
        }
        if !(hello.min_contract_version..=hello.max_contract_version)
            .contains(&RUNTIME_CLIENT_CONTRACT_VERSION)
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::UnsupportedContract,
                "Runtime client contract versions do not overlap",
            ));
        }
        let descriptor = Self::current();
        if !hello
            .required_capabilities
            .is_subset(&descriptor.capabilities)
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::UnsupportedContract,
                "Runtime does not provide every required client capability",
            ));
        }
        Ok(descriptor)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubmitRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub input: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubmitReceipt {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub owner_epoch: u64,
    /// The same actionable lifecycle boundary returned by event cursors. In
    /// particular, a pending approval is not exposed until the old execution
    /// owner has released the Run.
    pub state: RuntimeEventCursorState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionHead {
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub turn_count: u64,
    pub history_digest: String,
    pub active_run_id: Option<Uuid>,
}

impl From<LocalSessionHead> for RuntimeSessionHead {
    fn from(head: LocalSessionHead) -> Self {
        Self {
            session_id: head.session_id,
            branch_id: head.branch_id,
            generation: head.generation,
            turn_count: head.turn_count,
            history_digest: head.history_digest,
            active_run_id: head.active_run_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionTurnRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub run_id: Uuid,
    pub input: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionTurnReceipt {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub head: RuntimeSessionHead,
    pub run_id: Uuid,
    pub owner_epoch: Option<u64>,
    pub state: RuntimeEventCursorState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionForkRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub source_branch_id: Uuid,
    pub source_generation: u64,
    pub through_turn_ordinal: u64,
    pub target_branch_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionRollbackRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub through_turn_ordinal: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionReadRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionListRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub after_session_id: Option<Uuid>,
    pub after_branch_id: Option<Uuid>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionListPage {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub heads: Vec<RuntimeSessionHead>,
    pub next_after_session_id: Option<Uuid>,
    pub next_after_branch_id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionHistoryRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub after_turn_ordinal: u64,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionHistoryPage {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub turns: Vec<SessionConversationTurn>,
    pub next_after_turn_ordinal: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientEventCursorRequest {
    pub schema_version: u32,
    pub invocation: RuntimeInvocationContext,
    pub run_id: Uuid,
    pub after_sequence: u64,
    pub limit: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeClientErrorCode {
    InvalidRequest,
    UnsupportedContract,
    Forbidden,
    Conflict,
    ResourceExhausted,
    NotFound,
    CursorAhead,
    DataLoss,
    Unavailable,
    Internal,
}

/// Sanitized adapter-facing error. Host paths, provider details and credential
/// material never cross this boundary; trusted operators can still use the
/// lower-level Embedded Runtime diagnostics in their own logs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("Runtime client {code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientError {
    pub code: RuntimeClientErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientRecoveryFailure {
    pub invocation: RuntimeInvocationContext,
    pub error: RuntimeClientError,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeClientRecoveryReport {
    pub scanned_profiles: u64,
    pub recovered_runs: u64,
    pub failures: Vec<RuntimeClientRecoveryFailure>,
}

/// Keeps the reason on this side of the boundary.
///
/// A `StateRoot` error is a filesystem failure on the host's own path, and the
/// path must not cross the client contract -- which is why the reply says only
/// that storage is unavailable. The cost of that has been paid once already: a
/// Session test failed under load for weeks and the message could not say
/// whether it was an exhausted descriptor table, a transient ENOENT, or
/// something else. The reply stays sanitised and the host says what it saw.
fn note_state_root(operation: &str, error: &LocalRuntimeError) {
    tracing::warn!(%error, operation, "Session storage failed on the state root");
}

impl RuntimeClientError {
    fn new(code: RuntimeClientErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn from_embedded(error: EmbeddedRuntimeError) -> Self {
        match error {
            EmbeddedRuntimeError::UnregisteredInvocation => Self::new(
                RuntimeClientErrorCode::Forbidden,
                "this invocation is not registered",
            ),
            EmbeddedRuntimeError::ControlCommandRebound => Self::new(
                RuntimeClientErrorCode::Conflict,
                "this command id is already bound to a different command",
            ),
            EmbeddedRuntimeError::SessionTurnRebound => Self::new(
                RuntimeClientErrorCode::Conflict,
                "this Run id is already bound to a different Session Turn",
            ),
            // Not an error in the Runtime and not a malformed request: the Run
            // is simply not moving, so there is nothing for a steer to redirect.
            // `Conflict` rather than a new code -- this contract was stabilised
            // deliberately, and "your request conflicts with what this Run is
            // doing" is exactly what that code already means.
            EmbeddedRuntimeError::NotSteerable => Self::new(
                RuntimeClientErrorCode::Conflict,
                "this Run is not executing here, so there is nothing to steer",
            ),
            EmbeddedRuntimeError::InvalidControlCommand(_) => Self::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime control command",
            ),
            EmbeddedRuntimeError::Admission(RuntimeAdmissionError::Closed) => Self::new(
                RuntimeClientErrorCode::Unavailable,
                "the Runtime stopped before granting admission",
            ),
            EmbeddedRuntimeError::Admission(_) => Self::new(
                RuntimeClientErrorCode::ResourceExhausted,
                "the Runtime is at its admission ceiling",
            ),
            EmbeddedRuntimeError::EventCursor(cursor) => match cursor.code {
                RuntimeEventCursorErrorCode::UnsupportedSchema => Self::new(
                    RuntimeClientErrorCode::UnsupportedContract,
                    "unsupported event cursor schema",
                ),
                RuntimeEventCursorErrorCode::InvalidRequest => Self::new(
                    RuntimeClientErrorCode::InvalidRequest,
                    "invalid event cursor request",
                ),
                RuntimeEventCursorErrorCode::NotFound => {
                    Self::new(RuntimeClientErrorCode::NotFound, "no such Run")
                }
                RuntimeEventCursorErrorCode::CursorAhead => Self::new(
                    RuntimeClientErrorCode::CursorAhead,
                    "cursor is ahead of the committed log",
                ),
                RuntimeEventCursorErrorCode::IdentityMismatch => Self::new(
                    RuntimeClientErrorCode::Forbidden,
                    "this Run belongs to another invocation",
                ),
                RuntimeEventCursorErrorCode::CorruptLog => {
                    Self::new(RuntimeClientErrorCode::DataLoss, "the event log is corrupt")
                }
                RuntimeEventCursorErrorCode::StorageUnavailable => Self::new(
                    RuntimeClientErrorCode::Unavailable,
                    "the event log is unavailable",
                ),
            },
            EmbeddedRuntimeError::Configuration(_) | EmbeddedRuntimeError::Runtime(_) => Self::new(
                RuntimeClientErrorCode::Internal,
                "the Runtime could not complete this request",
            ),
        }
    }

    fn from_session_mutation(error: EmbeddedRuntimeError) -> Self {
        match error {
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::SessionCapacity(_)) => Self::new(
                RuntimeClientErrorCode::ResourceExhausted,
                "this Session store is at a ceiling",
            ),
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::Execution(_)) => Self::new(
                RuntimeClientErrorCode::Conflict,
                "Session head changed or is already active",
            ),
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::Checkpoint(_)) => Self::new(
                RuntimeClientErrorCode::DataLoss,
                "Session history is inconsistent",
            ),
            EmbeddedRuntimeError::Runtime(error @ LocalRuntimeError::StateRoot(_)) => {
                note_state_root("session_mutation", &error);
                Self::new(
                    RuntimeClientErrorCode::Unavailable,
                    "Session storage is unavailable",
                )
            }
            error => Self::from_embedded(error),
        }
    }

    fn from_session_read(error: EmbeddedRuntimeError) -> Self {
        match error {
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::SessionCapacity(_)) => Self::new(
                RuntimeClientErrorCode::ResourceExhausted,
                "this Session store is at a ceiling",
            ),
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::Execution(_)) => {
                Self::new(RuntimeClientErrorCode::NotFound, "no such Session branch")
            }
            EmbeddedRuntimeError::Runtime(LocalRuntimeError::Checkpoint(_)) => Self::new(
                RuntimeClientErrorCode::DataLoss,
                "Session history is inconsistent",
            ),
            EmbeddedRuntimeError::Runtime(error @ LocalRuntimeError::StateRoot(_)) => {
                note_state_root("session_read", &error);
                Self::new(
                    RuntimeClientErrorCode::Unavailable,
                    "Session storage is unavailable",
                )
            }
            error => Self::from_embedded(error),
        }
    }
}

/// Bounded client stream that keeps the Embedded subscription implementation
/// and its host-local errors behind the stable client contract.
pub struct RuntimeClientEventSubscription {
    inner: EmbeddedEventSubscription,
}

impl RuntimeClientEventSubscription {
    pub async fn recv(&mut self) -> Option<Result<RuntimeEventStreamItem, RuntimeClientError>> {
        self.inner
            .recv()
            .await
            .map(|result| result.map_err(RuntimeClientError::from_embedded))
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.inner.capacity() as u32
    }
}

/// Negotiation entry point for a Tauri command layer, an Electron sidecar
/// adapter, a CLI or the gRPC service. Only the initialized result exposes
/// execution methods; neither type exposes Runtime configuration, paths,
/// credentials, Host handles or mutable profile state.
#[derive(Clone)]
pub struct RuntimeClient {
    runtime: Arc<EmbeddedRuntime>,
}

impl RuntimeClient {
    #[must_use]
    pub fn new(runtime: Arc<EmbeddedRuntime>) -> Self {
        Self { runtime }
    }

    pub fn initialize(
        &self,
        hello: &RuntimeClientHello,
    ) -> Result<InitializedRuntimeClient, RuntimeClientError> {
        let descriptor = RuntimeClientDescriptor::negotiate(hello)?;
        Ok(InitializedRuntimeClient {
            runtime: Arc::clone(&self.runtime),
            descriptor,
        })
    }
}

/// A negotiated Runtime client. Execution methods exist only on this type, so
/// an in-process UI cannot accidentally skip version/capability negotiation
/// and then create durable Run state.
#[derive(Clone)]
pub struct InitializedRuntimeClient {
    runtime: Arc<EmbeddedRuntime>,
    descriptor: RuntimeClientDescriptor,
}

impl InitializedRuntimeClient {
    #[must_use]
    pub fn descriptor(&self) -> &RuntimeClientDescriptor {
        &self.descriptor
    }

    fn validate_session_identity(
        schema_version: u32,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
    ) -> Result<(), RuntimeClientError> {
        if schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || invocation.validate().is_err()
            || session_id.is_nil()
            || branch_id.is_nil()
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session request",
            ));
        }
        Ok(())
    }

    pub async fn start_session(
        &self,
        request: RuntimeSessionTurnRequest,
    ) -> Result<RuntimeSessionTurnReceipt, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.branch_id,
        )?;
        if request.generation != 1
            || request.run_id.is_nil()
            || request.input.trim().is_empty()
            || request.input.len() > RUNTIME_CLIENT_MAX_INPUT_BYTES
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session start",
            ));
        }
        let receipt = self
            .runtime
            .start_session_turn_detached(
                request.invocation,
                request.session_id,
                request.branch_id,
                request.run_id,
                request.input,
            )
            .await
            .map_err(RuntimeClientError::from_session_mutation)?;
        Ok(RuntimeSessionTurnReceipt {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: receipt.invocation,
            head: receipt.head.into(),
            run_id: receipt.run_id,
            owner_epoch: receipt.owner_epoch,
            state: receipt.state,
        })
    }

    pub async fn continue_session(
        &self,
        request: RuntimeSessionTurnRequest,
    ) -> Result<RuntimeSessionTurnReceipt, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.branch_id,
        )?;
        if request.generation == 0
            || request.run_id.is_nil()
            || request.input.trim().is_empty()
            || request.input.len() > RUNTIME_CLIENT_MAX_INPUT_BYTES
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session continuation",
            ));
        }
        let receipt = self
            .runtime
            .continue_session_turn_detached(
                request.invocation,
                request.session_id,
                request.branch_id,
                request.generation,
                request.run_id,
                request.input,
            )
            .await
            .map_err(RuntimeClientError::from_session_mutation)?;
        Ok(RuntimeSessionTurnReceipt {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: receipt.invocation,
            head: receipt.head.into(),
            run_id: receipt.run_id,
            owner_epoch: receipt.owner_epoch,
            state: receipt.state,
        })
    }

    pub async fn fork_session(
        &self,
        request: RuntimeSessionForkRequest,
    ) -> Result<RuntimeSessionHead, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.source_branch_id,
        )?;
        if request.source_generation == 0
            || request.target_branch_id.is_nil()
            || request.target_branch_id == request.source_branch_id
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session Fork",
            ));
        }
        self.runtime
            .fork_session(
                request.invocation,
                request.session_id,
                request.source_branch_id,
                request.source_generation,
                request.through_turn_ordinal,
                request.target_branch_id,
            )
            .await
            .map(RuntimeSessionHead::from)
            .map_err(RuntimeClientError::from_session_mutation)
    }

    pub async fn rollback_session(
        &self,
        request: RuntimeSessionRollbackRequest,
    ) -> Result<RuntimeSessionHead, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.branch_id,
        )?;
        // Both ends of the range. A rollback numbers the generation after the
        // one it names, so `u64::MAX` names a generation with no successor --
        // out of range in the same way zero is, and reported the same way
        // rather than as a conflict with a branch that did nothing wrong.
        if request.generation == 0 || request.generation == u64::MAX {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session Rollback",
            ));
        }
        self.runtime
            .rollback_session(
                request.invocation,
                request.session_id,
                request.branch_id,
                request.generation,
                request.through_turn_ordinal,
            )
            .await
            .map(RuntimeSessionHead::from)
            .map_err(RuntimeClientError::from_session_mutation)
    }

    pub fn read_session(
        &self,
        request: RuntimeSessionReadRequest,
    ) -> Result<RuntimeSessionHead, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.branch_id,
        )?;
        self.runtime
            .read_session_head(request.invocation, request.session_id, request.branch_id)
            .map(RuntimeSessionHead::from)
            .map_err(RuntimeClientError::from_session_read)
    }

    pub fn list_sessions(
        &self,
        request: RuntimeSessionListRequest,
    ) -> Result<RuntimeSessionListPage, RuntimeClientError> {
        if request.schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || request.invocation.validate().is_err()
            || !(1..=RUNTIME_CLIENT_MAX_SESSION_LIST_SIZE as u32).contains(&request.limit)
            || request.after_session_id.is_some() != request.after_branch_id.is_some()
            || request.after_session_id.is_some_and(|id| id.is_nil())
            || request.after_branch_id.is_some_and(|id| id.is_nil())
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session list request",
            ));
        }
        let page = self
            .runtime
            .list_session_heads(
                request.invocation,
                request.after_session_id.zip(request.after_branch_id),
                request.limit as usize,
            )
            .map_err(RuntimeClientError::from_session_read)?;
        let (next_after_session_id, next_after_branch_id) = page
            .next_after
            .map_or((None, None), |(session_id, branch_id)| {
                (Some(session_id), Some(branch_id))
            });
        Ok(RuntimeSessionListPage {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: page.invocation,
            heads: page.heads.into_iter().map(Into::into).collect(),
            next_after_session_id,
            next_after_branch_id,
        })
    }

    pub fn read_session_history(
        &self,
        request: RuntimeSessionHistoryRequest,
    ) -> Result<RuntimeSessionHistoryPage, RuntimeClientError> {
        Self::validate_session_identity(
            request.schema_version,
            request.invocation,
            request.session_id,
            request.branch_id,
        )?;
        if request.generation == 0
            || !(1..=RUNTIME_CLIENT_MAX_SESSION_HISTORY_TURNS as u32).contains(&request.limit)
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime Session history request",
            ));
        }
        let page = self
            .runtime
            .read_session_history(
                request.invocation,
                request.session_id,
                request.branch_id,
                request.generation,
                request.after_turn_ordinal,
                request.limit as usize,
            )
            .map_err(RuntimeClientError::from_session_read)?;
        Ok(RuntimeSessionHistoryPage {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: page.invocation,
            session_id: page.session_id,
            branch_id: page.branch_id,
            generation: page.generation,
            turns: page.turns,
            next_after_turn_ordinal: page.next_after_turn_ordinal,
        })
    }

    pub async fn submit(
        &self,
        request: RuntimeSubmitRequest,
    ) -> Result<RuntimeSubmitReceipt, RuntimeClientError> {
        if request.schema_version != RUNTIME_CLIENT_SCHEMA_VERSION
            || request.run_id.is_nil()
            || request.input.trim().is_empty()
            || request.input.len() > RUNTIME_CLIENT_MAX_INPUT_BYTES
            || request.invocation.validate().is_err()
        {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime submit request",
            ));
        }
        let record = self
            .runtime
            .execute_detached(request.invocation, request.run_id, request.input)
            .await
            .map_err(RuntimeClientError::from_embedded)?;
        let page = self
            .runtime
            .event_cursor(RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: request.invocation,
                run_id: request.run_id,
                after_sequence: 0,
                limit: 1,
            })
            .map_err(RuntimeClientError::from_embedded)?;
        Ok(RuntimeSubmitReceipt {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: request.invocation,
            run_id: record.run_id,
            owner_epoch: record.owner_epoch,
            state: page.state,
        })
    }

    pub async fn control(
        &self,
        command: RuntimeControlCommand,
    ) -> Result<RuntimeControlReceipt, RuntimeClientError> {
        let action_bytes = serde_json::to_vec(&command.action).map_err(|_| {
            RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "invalid Runtime control action",
            )
        })?;
        if action_bytes.len() > RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES {
            return Err(RuntimeClientError::new(
                RuntimeClientErrorCode::InvalidRequest,
                "Runtime control action exceeds its bound",
            ));
        }
        self.runtime
            .control_detached(command)
            .await
            .map_err(RuntimeClientError::from_embedded)
    }

    pub fn read_events(
        &self,
        request: RuntimeClientEventCursorRequest,
    ) -> Result<RuntimeEventCursorPage, RuntimeClientError> {
        self.runtime
            .event_cursor(RuntimeEventCursorRequest {
                schema_version: request.schema_version,
                invocation: request.invocation,
                run_id: request.run_id,
                after_sequence: request.after_sequence,
                limit: request.limit as usize,
            })
            .map_err(RuntimeClientError::from_embedded)
    }

    pub fn watch_events(
        &self,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        after_sequence: u64,
        capacity: u32,
    ) -> Result<RuntimeClientEventSubscription, RuntimeClientError> {
        let inner = self
            .runtime
            .subscribe_events(invocation, run_id, after_sequence, capacity as usize)
            .map_err(RuntimeClientError::from_embedded)?;
        Ok(RuntimeClientEventSubscription { inner })
    }

    pub async fn recover_on_startup(&self) -> RuntimeClientRecoveryReport {
        let report = self.runtime.recover_all_unfinished_detached().await;
        RuntimeClientRecoveryReport {
            scanned_profiles: report.scanned_profiles as u64,
            recovered_runs: report.recovered_runs as u64,
            failures: report
                .failures
                .into_iter()
                .map(|failure| RuntimeClientRecoveryFailure {
                    invocation: failure.invocation,
                    error: RuntimeClientError::from_embedded(failure.error),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(required_capabilities: &[&str]) -> RuntimeClientHello {
        RuntimeClientHello {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            min_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            max_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            required_capabilities: required_capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        }
    }

    #[test]
    fn initialization_negotiates_one_deterministic_bounded_contract() {
        let descriptor = RuntimeClientDescriptor::negotiate(&hello(&[
            RUNTIME_CAPABILITY_RUN_SUBMIT,
            RUNTIME_CAPABILITY_EVENTS_WATCH,
        ]))
        .expect("compatible client");

        assert_eq!(descriptor.contract_version, 1);
        // Written out rather than derived from the constants on purpose. This
        // list is the published surface: adding to it is a deliberate act that
        // every other client has to be told about, and a rename is a break even
        // when the constant still compiles.
        assert_eq!(
            descriptor.capabilities.into_iter().collect::<Vec<_>>(),
            vec![
                "events.cursor.v1",
                "events.watch.v1",
                "recovery.startup.v1",
                "run.control.v1",
                "run.submit.v1",
                "session.continue.v1",
                "session.fork.v1",
                "session.history.v1",
                "session.list.v1",
                "session.read.v1",
                "session.rollback.v1",
                "session.start.v1",
            ]
        );
        assert_eq!(descriptor.max_input_bytes, 32_000);
        assert_eq!(
            descriptor.max_event_page_size,
            RUNTIME_EVENT_CURSOR_MAX_EVENTS as u32
        );
        // Bounded before a caller asks. A client that has to discover a page
        // ceiling by being rejected will discover it in production.
        assert_eq!(descriptor.max_session_list_size, 256);
        assert_eq!(descriptor.max_session_history_turns, 128);
    }

    #[test]
    fn initialization_refuses_version_or_capability_guessing() {
        let mut incompatible = hello(&[]);
        incompatible.min_contract_version = 2;
        incompatible.max_contract_version = 3;
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&incompatible)
                .expect_err("non-overlap")
                .code,
            RuntimeClientErrorCode::UnsupportedContract
        );

        let missing = hello(&["desktop.magic.v1"]);
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&missing)
                .expect_err("missing capability")
                .code,
            RuntimeClientErrorCode::UnsupportedContract
        );

        let invalid = hello(&["UPPERCASE"]);
        assert_eq!(
            RuntimeClientDescriptor::negotiate(&invalid)
                .expect_err("invalid capability token")
                .code,
            RuntimeClientErrorCode::InvalidRequest
        );
    }

    #[test]
    fn stable_client_errors_do_not_expose_host_paths() {
        let error = RuntimeClientError::from_embedded(EmbeddedRuntimeError::Configuration(
            "failed to read /Users/private/.secrets/provider-key".into(),
        ));
        assert_eq!(error.code, RuntimeClientErrorCode::Internal);
        assert!(!error.message.contains("/Users/private"));
        assert!(!error.message.contains("provider-key"));
    }
}
