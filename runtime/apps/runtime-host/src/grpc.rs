//! The Runtime's own network invocation surface (ADR-0123).
//!
//! Thin on purpose, like the Model Gateway's gRPC services. Admission, owner
//! epochs, durable control receipts, retention and the event cursor all already
//! live in [`crate::embedded`]; this layer only authenticates and translates. A
//! rule implemented here instead would be a rule the local adapter and the
//! embedded tests could not reach, and the two surfaces would drift.
//!
//! The security property this layer owns is one sentence: **tenant, application
//! and workload identity come from the verified token, never from the request
//! body.** `RuntimeControlCommand` carries its own invocation, and its doc
//! comment says authentication belongs to the adapter -- this is that adapter.

use crate::LocalRunState;
use crate::embedded::RuntimeEventStreamItem;
use crate::embedded::{
    EmbeddedRuntime, EmbeddedRuntimeError, RuntimeControlAction, RuntimeControlCommand,
    RuntimeControlReceiptState, RuntimeEventCursorErrorCode, RuntimeEventCursorRequest,
    RuntimeEventCursorState,
};
use agent_protocol::{RUNTIME_INVOCATION_SCHEMA_VERSION, RunStatus, RuntimeInvocationContext};
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocation;
use agent_runtime_invocation_protocol::v1::{
    ControlReceiptState, ControlRunRequest, ControlRunResponse, ReadRunEventsRequest,
    ReadRunEventsResponse, RunEventBoundary, RunEventStreamItem, RunLifecycleBoundary,
    RuntimeEvent, RuntimeInvocationRef, SubmitRunRequest, SubmitRunResponse, WatchRunEventsRequest,
    run_event_stream_item, run_lifecycle_boundary,
};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadTokenError, WorkloadTokenVerifier,
};
use chrono::Utc;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

const CONTROL_SCHEMA_VERSION: u32 = 1;
const EVENT_PAGE_SCHEMA_VERSION: u32 = 1;

/// Invoking a Runtime is its own capability.
///
/// Deliberately not `mcp.federate` or `mcp.oauth.admin`: a token that may drive
/// a tenant's tools, or administer its credentials, is not automatically a
/// token that may start Runs and spend that tenant's budget. Keeping them
/// separate is what lets a later policy grant one without the other.
const RUNTIME_INVOKE_SCOPE: &str = "runtime.invoke";

const RUNTIME_AUDIENCE: &str = "runtime-host";

/// Fail-closed bounds. tonic's own message ceiling is a backstop, not a
/// contract; a surface that accepts whatever arrives has no stated limit.
const MAX_ACTION_JSON_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

pub struct RuntimeInvocationGrpcService {
    runtime: Arc<EmbeddedRuntime>,
    verifier: WorkloadTokenVerifier,
}

impl RuntimeInvocationGrpcService {
    #[must_use]
    pub fn new(runtime: Arc<EmbeddedRuntime>, verifier: WorkloadTokenVerifier) -> Self {
        Self { runtime, verifier }
    }

    /// Verifies the bearer token and resolves the invocation the caller may act
    /// as.
    ///
    /// The three identity fields are read from the claims and the request may
    /// only *agree* with them. The remaining three -- workspace, agent version,
    /// model policy -- come from the request, because they select a Profile
    /// rather than assert an identity. That is safe because a Profile is keyed
    /// on the whole six-tuple: a token pinned to tenant A cannot reach a
    /// Profile registered for tenant B no matter what it names here.
    fn authenticate<T>(
        &self,
        request: &Request<T>,
        asserted: Option<&RuntimeInvocationRef>,
    ) -> Result<RuntimeInvocationContext, Status> {
        let asserted =
            asserted.ok_or_else(|| Status::invalid_argument("missing invocation reference"))?;
        if asserted.schema_version != RUNTIME_INVOCATION_SCHEMA_VERSION {
            return Err(Status::invalid_argument(
                "unsupported Runtime invocation schema version",
            ));
        }
        let bearer = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("missing workload bearer token"))?;
        let claims = self
            .verifier
            .verify(
                bearer,
                // An external caller is not a Run. Asking the contract for the
                // operator shape means a Run-shaped token carrying this scope
                // is refused by its shape, without this service remembering to
                // check (ADR-0121).
                RequiredCapability::operator(RUNTIME_AUDIENCE, RUNTIME_INVOKE_SCOPE),
                Utc::now().timestamp_millis(),
            )
            .map_err(|error| match error {
                // A valid token of the wrong kind is not an authentication
                // problem, and no scope grant turns a Run into an operator.
                WorkloadTokenError::WrongIdentityShape => Status::permission_denied(
                    "the Runtime invocation surface requires an operator identity",
                ),
                _ => Status::unauthenticated("invalid workload token"),
            })?;
        let binding = WorkloadIdentityBinding {
            tenant_id: parse_uuid(&asserted.tenant_id, "tenant_id")?,
            application_id: parse_uuid(&asserted.application_id, "application_id")?,
            workload_identity_id: parse_uuid(
                &asserted.workload_identity_id,
                "workload_identity_id",
            )?,
            // Taken from the claims, so the body cannot widen what the token
            // already says. For an operator token these are all nil.
            run_id: claims.run_id,
            session_id: claims.session_id,
            workspace_id: claims.workspace_id,
            agent_version_id: claims.agent_version_id,
            attempt_id: claims.attempt_id,
            worker_id: claims.worker_id,
            worker_incarnation_id: claims.worker_incarnation_id,
        };
        if !claims.authorizes(&binding) {
            return Err(Status::permission_denied(
                "workload token does not authorize this tenant, application or workload identity",
            ));
        }
        Ok(RuntimeInvocationContext {
            schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
            tenant_id: binding.tenant_id,
            application_id: binding.application_id,
            workload_identity_id: binding.workload_identity_id,
            workspace_id: parse_uuid(&asserted.workspace_id, "workspace_id")?,
            agent_version_id: parse_uuid(&asserted.agent_version_id, "agent_version_id")?,
            model_policy_id: parse_uuid(&asserted.model_policy_id, "model_policy_id")?,
        })
    }
}

#[tonic::async_trait]
impl RuntimeInvocation for RuntimeInvocationGrpcService {
    async fn submit(
        &self,
        request: Request<SubmitRunRequest>,
    ) -> Result<Response<SubmitRunResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        if message.input.len() > MAX_INPUT_BYTES {
            return Err(Status::invalid_argument("run input exceeds its bound"));
        }
        // Caller-chosen so a retried Submit reaches the same Run instead of
        // starting a second one.
        let run_id = parse_uuid(&message.run_id, "run_id")?;
        let input = message.input.clone();

        let record = self
            .runtime
            .execute_detached(invocation, run_id, input)
            .await
            .map_err(runtime_status)?;

        Ok(Response::new(SubmitRunResponse {
            run_id: record.run_id.to_string(),
            owner_epoch: record.owner_epoch,
            status: run_state_token(&record.state),
        }))
    }

    async fn control(
        &self,
        request: Request<ControlRunRequest>,
    ) -> Result<Response<ControlRunResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        if message.schema_version != CONTROL_SCHEMA_VERSION {
            return Err(Status::invalid_argument(
                "unsupported Runtime control schema version",
            ));
        }
        if message.action_json.len() > MAX_ACTION_JSON_BYTES {
            return Err(Status::invalid_argument("control action exceeds its bound"));
        }
        let action: RuntimeControlAction = serde_json::from_slice(&message.action_json)
            .map_err(|_| Status::invalid_argument("control action is not a known action"))?;

        let command = RuntimeControlCommand {
            schema_version: crate::embedded::RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: parse_uuid(&message.command_id, "command_id")?,
            invocation,
            run_id: parse_uuid(&message.run_id, "run_id")?,
            expected_owner_epoch: message.expected_owner_epoch,
            action,
        };

        let receipt = self
            .runtime
            .control_detached(command)
            .await
            .map_err(runtime_status)?;

        Ok(Response::new(ControlRunResponse {
            command_id: receipt.command_id.to_string(),
            command_digest: receipt.command_digest,
            run_id: receipt.run_id.to_string(),
            expected_owner_epoch: receipt.expected_owner_epoch,
            applied_owner_epoch: receipt.applied_owner_epoch,
            state: match receipt.state {
                RuntimeControlReceiptState::Accepted => ControlReceiptState::Accepted,
                RuntimeControlReceiptState::Completed => ControlReceiptState::Completed,
            } as i32,
            run_status: receipt.run_status.map(status_token).unwrap_or_default(),
        }))
    }

    type WatchEventsStream =
        Pin<Box<dyn Stream<Item = Result<RunEventStreamItem, Status>> + Send + 'static>>;

    /// Follows a Run from the durable log.
    ///
    /// The subscription is pumped into a bounded channel rather than polled by
    /// the client: a follower that stops reading applies backpressure here
    /// instead of accumulating an unbounded queue in the Runtime. Dropping the
    /// stream drops the receiver, which ends the pump on its next send.
    ///
    /// Cursor semantics are the same exclusive ones `ReadEvents` uses, so a
    /// dropped stream is resumed by reconnecting with the last sequence seen.
    async fn watch_events(
        &self,
        request: Request<WatchRunEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        if message.schema_version != EVENT_PAGE_SCHEMA_VERSION {
            return Err(Status::invalid_argument(
                "unsupported Runtime event page schema version",
            ));
        }
        let run_id = parse_uuid(&message.run_id, "run_id")?;
        let capacity = message.capacity as usize;
        // Rejected, not clamped: a caller that asked for a buffer it will not
        // get should learn that rather than silently receive another one.
        let mut subscription = self
            .runtime
            .subscribe_events(invocation, run_id, message.after_sequence, capacity)
            .map_err(runtime_status)?;

        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1));
        tokio::spawn(async move {
            while let Some(item) = subscription.recv().await {
                let message = match item {
                    Ok(item) => Ok(wire_stream_item(item)),
                    Err(error) => Err(runtime_status(error)),
                };
                let failed = message.is_err();
                if sender.send(message).await.is_err() || failed {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn read_events(
        &self,
        request: Request<ReadRunEventsRequest>,
    ) -> Result<Response<ReadRunEventsResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        if message.schema_version != EVENT_PAGE_SCHEMA_VERSION {
            return Err(Status::invalid_argument(
                "unsupported Runtime event page schema version",
            ));
        }

        let page = self
            .runtime
            .event_cursor(RuntimeEventCursorRequest {
                schema_version: crate::embedded::RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation,
                run_id: parse_uuid(&message.run_id, "run_id")?,
                after_sequence: message.after_sequence,
                limit: message.limit as usize,
            })
            .map_err(runtime_status)?;

        Ok(Response::new(ReadRunEventsResponse {
            schema_version: EVENT_PAGE_SCHEMA_VERSION,
            run_id: page.run_id.to_string(),
            requested_after_sequence: page.requested_after_sequence,
            next_after_sequence: page.next_after_sequence,
            earliest_available_sequence: page.earliest_available_sequence,
            highest_committed_sequence: page.highest_committed_sequence,
            history_gap: page.history_gap,
            has_more: page.has_more,
            boundary: Some(wire_boundary(&page.state)),
            events: page.events.iter().map(wire_event).collect(),
        }))
    }
}

fn wire_event(event: &crate::LocalEvent) -> RuntimeEvent {
    RuntimeEvent {
        event_id: event.event_id.to_string(),
        tenant_id: event.tenant_id.to_string(),
        session_id: event.session_id.to_string(),
        run_id: event.run_id.to_string(),
        sequence: event.sequence,
        attempt_id: event.attempt_id.to_string(),
        occurred_at_unix_ms: event.timestamp.timestamp_millis(),
        trace_id: event.trace_id.to_string(),
        r#type: event.event_type.clone(),
        payload_json: event.payload.to_string().into_bytes(),
        digest: event.digest.clone(),
    }
}

/// Keeps the lifecycle boundary typed across the wire (ADR-0114). A caller that
/// has drained a page must be able to tell "nothing more yet" from "nothing
/// more ever" without parsing event payloads.
fn wire_boundary(state: &RuntimeEventCursorState) -> RunLifecycleBoundary {
    use run_lifecycle_boundary as wire;
    let boundary = match state {
        RuntimeEventCursorState::Running => wire::Boundary::Running(wire::Running {}),
        RuntimeEventCursorState::Cancelling => wire::Boundary::Cancelling(wire::Cancelling {}),
        RuntimeEventCursorState::WaitingApproval => {
            wire::Boundary::WaitingApproval(wire::WaitingApproval {})
        }
        RuntimeEventCursorState::Suspended => wire::Boundary::Suspended(wire::Suspended {}),
        RuntimeEventCursorState::Interrupted => wire::Boundary::Interrupted(wire::Interrupted {}),
        RuntimeEventCursorState::Terminal { status } => wire::Boundary::Terminal(wire::Terminal {
            status: status_token(*status),
        }),
        RuntimeEventCursorState::Retired {
            status,
            terminal_event_id,
            terminal_sequence,
            terminal_event_digest,
        } => wire::Boundary::Retired(wire::Retired {
            status: status_token(*status),
            terminal_event_id: terminal_event_id.to_string(),
            terminal_sequence: *terminal_sequence,
            terminal_event_digest: terminal_event_digest.clone(),
        }),
    };
    RunLifecycleBoundary {
        boundary: Some(boundary),
    }
}

/// Maps a Runtime failure to a status **without** passing the internal message
/// through.
///
/// `LocalRuntimeError` and `Configuration` carry state-root paths and other
/// host-local detail. A network caller gets the typed outcome and nothing that
/// describes this machine.
fn runtime_status(error: EmbeddedRuntimeError) -> Status {
    match error {
        // Deliberately not `not_found`: whether a Profile exists is not
        // something an unauthorized caller should be able to probe, and to an
        // authorized one the answer is the same either way -- it may not
        // invoke this.
        EmbeddedRuntimeError::UnregisteredInvocation => {
            Status::permission_denied("this invocation is not registered")
        }
        // The caller reused an idempotency key for a different action. It is
        // actionable and it is theirs, so it must not arrive as `internal`.
        EmbeddedRuntimeError::ControlCommandRebound => {
            Status::failed_precondition("this command id is already bound to a different command")
        }
        EmbeddedRuntimeError::InvalidControlCommand(_) => {
            Status::invalid_argument("invalid Runtime control command")
        }
        EmbeddedRuntimeError::Admission(_) => {
            Status::resource_exhausted("the Runtime is at its admission ceiling")
        }
        EmbeddedRuntimeError::EventCursor(cursor) => match cursor.code {
            RuntimeEventCursorErrorCode::UnsupportedSchema => {
                Status::failed_precondition("unsupported event cursor schema")
            }
            RuntimeEventCursorErrorCode::InvalidRequest => {
                Status::invalid_argument("invalid event cursor request")
            }
            RuntimeEventCursorErrorCode::NotFound => Status::not_found("no such Run"),
            RuntimeEventCursorErrorCode::CursorAhead => {
                Status::out_of_range("cursor is ahead of the committed log")
            }
            RuntimeEventCursorErrorCode::IdentityMismatch => {
                Status::permission_denied("this Run belongs to another invocation")
            }
            RuntimeEventCursorErrorCode::CorruptLog => {
                Status::data_loss("the event log is corrupt")
            }
            RuntimeEventCursorErrorCode::StorageUnavailable => {
                Status::unavailable("the event log is unavailable")
            }
        },
        EmbeddedRuntimeError::Configuration(_) | EmbeddedRuntimeError::Runtime(_) => {
            Status::internal("the Runtime could not complete this request")
        }
    }
}

fn wire_stream_item(item: RuntimeEventStreamItem) -> RunEventStreamItem {
    let item = match item {
        RuntimeEventStreamItem::Event { event, .. } => {
            run_event_stream_item::Item::Event(wire_event(&event))
        }
        RuntimeEventStreamItem::Boundary {
            next_after_sequence,
            earliest_available_sequence,
            highest_committed_sequence,
            history_gap,
            state,
            ..
        } => run_event_stream_item::Item::Boundary(RunEventBoundary {
            next_after_sequence,
            earliest_available_sequence,
            highest_committed_sequence,
            history_gap,
            lifecycle: Some(wire_boundary(&state)),
        }),
    };
    RunEventStreamItem { item: Some(item) }
}

fn parse_uuid(value: &str, field: &'static str) -> Result<Uuid, Status> {
    Uuid::parse_str(value).map_err(|_| Status::invalid_argument(format!("{field} is not a UUID")))
}

/// The canonical token for a status, taken from its serde representation.
///
/// Not `format!("{:?}")`: `RunStatus::WaitingApproval` debug-lowercases to
/// `waitingapproval`, while every other surface in this system spells it
/// `waiting_approval`. A wire contract built on `Debug` also silently changes
/// whenever a variant is renamed.
fn status_token(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The state tag only, never the data a variant carries.
///
/// `LocalRunState` is `#[serde(tag = "state")]`, so `Cancelling { reason }`
/// debug-formats the operator's reason text straight into what this field
/// documents as a status token. Reading the tag drops it.
fn run_state_token(state: &LocalRunState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| {
            value
                .get("state")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}
