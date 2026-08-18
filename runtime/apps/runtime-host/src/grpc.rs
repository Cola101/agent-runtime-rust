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

use crate::client::{
    InitializedRuntimeClient, RUNTIME_CAPABILITY_EVENTS_CURSOR, RUNTIME_CAPABILITY_EVENTS_WATCH,
    RUNTIME_CAPABILITY_RECOVERY_STARTUP, RUNTIME_CAPABILITY_RUN_CONTROL,
    RUNTIME_CAPABILITY_RUN_SUBMIT, RUNTIME_CAPABILITY_SESSION_CONTINUE,
    RUNTIME_CAPABILITY_SESSION_FORK, RUNTIME_CAPABILITY_SESSION_HISTORY,
    RUNTIME_CAPABILITY_SESSION_LIST, RUNTIME_CAPABILITY_SESSION_READ,
    RUNTIME_CAPABILITY_SESSION_ROLLBACK, RUNTIME_CAPABILITY_SESSION_START,
    RUNTIME_CLIENT_CONTRACT_VERSION, RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES,
    RUNTIME_CLIENT_MAX_INPUT_BYTES, RUNTIME_CLIENT_SCHEMA_VERSION, RuntimeClient,
    RuntimeClientError, RuntimeClientErrorCode, RuntimeClientEventCursorRequest,
    RuntimeClientHello, RuntimeSessionForkRequest, RuntimeSessionHistoryRequest,
    RuntimeSessionListRequest, RuntimeSessionReadRequest, RuntimeSessionRollbackRequest,
    RuntimeSessionTurnRequest, RuntimeSubmitRequest,
};
use crate::embedded::RuntimeEventStreamItem;
use crate::embedded::{
    EmbeddedRuntime, RuntimeControlAction, RuntimeControlCommand, RuntimeControlReceiptState,
    RuntimeEventCursorState,
};
use agent_protocol::{RUNTIME_INVOCATION_SCHEMA_VERSION, RunStatus, RuntimeInvocationContext};
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocation;
use agent_runtime_invocation_protocol::v1::{
    ControlReceiptState, ControlRunRequest, ControlRunResponse, ForkSessionRequest,
    InitializeRuntimeRequest, InitializeRuntimeResponse, ListSessionsRequest, ListSessionsResponse,
    ReadRunEventsRequest, ReadRunEventsResponse, ReadSessionHistoryRequest,
    ReadSessionHistoryResponse, ReadSessionRequest, RollbackSessionRequest, RunEventBoundary,
    RunEventStreamItem, RunLifecycleBoundary, RuntimeEvent, RuntimeInvocationRef,
    SessionConversationTurn as WireSessionConversationTurn, SessionHead, SessionTurnRequest,
    SessionTurnResponse, SubmitRunRequest, SubmitRunResponse, WatchRunEventsRequest,
    run_event_stream_item, run_lifecycle_boundary,
};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadTokenError, WorkloadTokenVerifier,
};
use chrono::Utc;
use std::collections::BTreeSet;
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

pub struct RuntimeInvocationGrpcService {
    client_port: RuntimeClient,
    client: InitializedRuntimeClient,
    verifier: WorkloadTokenVerifier,
}

impl RuntimeInvocationGrpcService {
    #[must_use]
    pub fn new(runtime: Arc<EmbeddedRuntime>, verifier: WorkloadTokenVerifier) -> Self {
        let client_port = RuntimeClient::new(runtime);
        let client = client_port
            .initialize(&RuntimeClientHello {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                min_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
                max_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
                required_capabilities: BTreeSet::from([
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
            })
            .expect("the gRPC adapter and Runtime client contract are compiled together");
        Self {
            client_port,
            client,
            verifier,
        }
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
    async fn initialize(
        &self,
        request: Request<InitializeRuntimeRequest>,
    ) -> Result<Response<InitializeRuntimeResponse>, Status> {
        let message = request.get_ref();
        let client = self
            .client_port
            .initialize(&RuntimeClientHello {
                schema_version: message.schema_version,
                min_contract_version: message.min_contract_version,
                max_contract_version: message.max_contract_version,
                required_capabilities: message
                    .required_capabilities
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            })
            .map_err(runtime_client_status)?;
        let descriptor = client.descriptor();
        Ok(Response::new(InitializeRuntimeResponse {
            schema_version: descriptor.schema_version,
            contract_version: descriptor.contract_version,
            runtime_version: descriptor.runtime_version.clone(),
            capabilities: descriptor.capabilities.iter().cloned().collect(),
            max_input_bytes: descriptor.max_input_bytes,
            max_action_json_bytes: descriptor.max_action_json_bytes,
            max_event_page_size: descriptor.max_event_page_size,
            max_event_stream_capacity: descriptor.max_event_stream_capacity,
            max_session_list_size: descriptor.max_session_list_size,
            max_session_history_turns: descriptor.max_session_history_turns,
        }))
    }

    async fn submit(
        &self,
        request: Request<SubmitRunRequest>,
    ) -> Result<Response<SubmitRunResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        if message.input.len() > RUNTIME_CLIENT_MAX_INPUT_BYTES {
            return Err(Status::invalid_argument("run input exceeds its bound"));
        }
        // Caller-chosen so a retried Submit reaches the same Run instead of
        // starting a second one.
        let run_id = parse_uuid(&message.run_id, "run_id")?;
        let receipt = self
            .client
            .submit(RuntimeSubmitRequest {
                schema_version: crate::client::RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation,
                run_id,
                input: message.input.clone(),
            })
            .await
            .map_err(runtime_client_status)?;

        Ok(Response::new(SubmitRunResponse {
            run_id: receipt.run_id.to_string(),
            owner_epoch: receipt.owner_epoch,
            status: boundary_status_token(&receipt.state),
        }))
    }

    async fn start_session(
        &self,
        request: Request<SessionTurnRequest>,
    ) -> Result<Response<SessionTurnResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let receipt = self
            .client
            .start_session(RuntimeSessionTurnRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                branch_id: parse_uuid(&message.branch_id, "branch_id")?,
                generation: message.generation,
                run_id: parse_uuid(&message.run_id, "run_id")?,
                input: message.input.clone(),
            })
            .await
            .map_err(runtime_client_status)?;
        Ok(Response::new(SessionTurnResponse {
            schema_version: receipt.schema_version,
            head: Some(wire_session_head(&receipt.head)),
            run_id: receipt.run_id.to_string(),
            owner_epoch: receipt.owner_epoch,
            boundary: Some(wire_boundary(&receipt.state)),
        }))
    }

    async fn continue_session(
        &self,
        request: Request<SessionTurnRequest>,
    ) -> Result<Response<SessionTurnResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let receipt = self
            .client
            .continue_session(RuntimeSessionTurnRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                branch_id: parse_uuid(&message.branch_id, "branch_id")?,
                generation: message.generation,
                run_id: parse_uuid(&message.run_id, "run_id")?,
                input: message.input.clone(),
            })
            .await
            .map_err(runtime_client_status)?;
        Ok(Response::new(SessionTurnResponse {
            schema_version: receipt.schema_version,
            head: Some(wire_session_head(&receipt.head)),
            run_id: receipt.run_id.to_string(),
            owner_epoch: receipt.owner_epoch,
            boundary: Some(wire_boundary(&receipt.state)),
        }))
    }

    async fn fork_session(
        &self,
        request: Request<ForkSessionRequest>,
    ) -> Result<Response<SessionHead>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let head = self
            .client
            .fork_session(RuntimeSessionForkRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                source_branch_id: parse_uuid(&message.source_branch_id, "source_branch_id")?,
                source_generation: message.source_generation,
                through_turn_ordinal: message.through_turn_ordinal,
                target_branch_id: parse_uuid(&message.target_branch_id, "target_branch_id")?,
            })
            .await
            .map_err(runtime_client_status)?;
        Ok(Response::new(wire_session_head(&head)))
    }

    async fn rollback_session(
        &self,
        request: Request<RollbackSessionRequest>,
    ) -> Result<Response<SessionHead>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let head = self
            .client
            .rollback_session(RuntimeSessionRollbackRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                branch_id: parse_uuid(&message.branch_id, "branch_id")?,
                generation: message.generation,
                through_turn_ordinal: message.through_turn_ordinal,
            })
            .await
            .map_err(runtime_client_status)?;
        Ok(Response::new(wire_session_head(&head)))
    }

    async fn read_session(
        &self,
        request: Request<ReadSessionRequest>,
    ) -> Result<Response<SessionHead>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let head = self
            .client
            .read_session(RuntimeSessionReadRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                branch_id: parse_uuid(&message.branch_id, "branch_id")?,
            })
            .map_err(runtime_client_status)?;
        Ok(Response::new(wire_session_head(&head)))
    }

    async fn list_sessions(
        &self,
        request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let page = self
            .client
            .list_sessions(RuntimeSessionListRequest {
                schema_version: message.schema_version,
                invocation,
                after_session_id: message
                    .after_session_id
                    .as_deref()
                    .map(|value| parse_uuid(value, "after_session_id"))
                    .transpose()?,
                after_branch_id: message
                    .after_branch_id
                    .as_deref()
                    .map(|value| parse_uuid(value, "after_branch_id"))
                    .transpose()?,
                limit: message.limit,
            })
            .map_err(runtime_client_status)?;
        Ok(Response::new(ListSessionsResponse {
            schema_version: page.schema_version,
            heads: page.heads.iter().map(wire_session_head).collect(),
            next_after_session_id: page.next_after_session_id.map(|id| id.to_string()),
            next_after_branch_id: page.next_after_branch_id.map(|id| id.to_string()),
        }))
    }

    async fn read_session_history(
        &self,
        request: Request<ReadSessionHistoryRequest>,
    ) -> Result<Response<ReadSessionHistoryResponse>, Status> {
        let invocation = self.authenticate(&request, request.get_ref().invocation.as_ref())?;
        let message = request.get_ref();
        let page = self
            .client
            .read_session_history(RuntimeSessionHistoryRequest {
                schema_version: message.schema_version,
                invocation,
                session_id: parse_uuid(&message.session_id, "session_id")?,
                branch_id: parse_uuid(&message.branch_id, "branch_id")?,
                generation: message.generation,
                after_turn_ordinal: message.after_turn_ordinal,
                limit: message.limit,
            })
            .map_err(runtime_client_status)?;
        let turns = page
            .turns
            .iter()
            .map(|turn| {
                serde_json::to_vec(turn)
                    .map(|turn_json| WireSessionConversationTurn {
                        turn_ordinal: turn.turn_ordinal,
                        run_id: turn.run_id.to_string(),
                        turn_json,
                    })
                    .map_err(|_| Status::internal("Session Turn could not be encoded"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(ReadSessionHistoryResponse {
            schema_version: page.schema_version,
            session_id: page.session_id.to_string(),
            branch_id: page.branch_id.to_string(),
            generation: page.generation,
            turns,
            next_after_turn_ordinal: page.next_after_turn_ordinal,
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
        if message.action_json.len() > RUNTIME_CLIENT_MAX_ACTION_JSON_BYTES {
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
            .client
            .control(command)
            .await
            .map_err(runtime_client_status)?;

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
            .client
            .watch_events(invocation, run_id, message.after_sequence, message.capacity)
            .map_err(runtime_client_status)?;

        let (sender, receiver) = tokio::sync::mpsc::channel(capacity.max(1));
        tokio::spawn(async move {
            while let Some(item) = subscription.recv().await {
                let message = match item {
                    Ok(item) => Ok(wire_stream_item(item)),
                    Err(error) => Err(runtime_client_status(error)),
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
            .client
            .read_events(RuntimeClientEventCursorRequest {
                schema_version: crate::embedded::RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation,
                run_id: parse_uuid(&message.run_id, "run_id")?,
                after_sequence: message.after_sequence,
                limit: message.limit,
            })
            .map_err(runtime_client_status)?;

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

fn wire_session_head(head: &crate::client::RuntimeSessionHead) -> SessionHead {
    SessionHead {
        session_id: head.session_id.to_string(),
        branch_id: head.branch_id.to_string(),
        generation: head.generation,
        turn_count: head.turn_count,
        history_digest: head.history_digest.clone(),
        active_run_id: head.active_run_id.map(|id| id.to_string()),
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

/// Maps the already-sanitized stable client error into its wire status.
fn runtime_client_status(error: RuntimeClientError) -> Status {
    match error.code {
        RuntimeClientErrorCode::InvalidRequest => Status::invalid_argument(error.message),
        RuntimeClientErrorCode::UnsupportedContract => Status::failed_precondition(error.message),
        RuntimeClientErrorCode::Forbidden => Status::permission_denied(error.message),
        RuntimeClientErrorCode::Conflict => Status::failed_precondition(error.message),
        RuntimeClientErrorCode::ResourceExhausted => Status::resource_exhausted(error.message),
        RuntimeClientErrorCode::NotFound => Status::not_found(error.message),
        RuntimeClientErrorCode::CursorAhead => Status::out_of_range(error.message),
        RuntimeClientErrorCode::DataLoss => Status::data_loss(error.message),
        RuntimeClientErrorCode::Unavailable => Status::unavailable(error.message),
        RuntimeClientErrorCode::Internal => Status::internal(error.message),
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

/// Submit returns the same actionable lifecycle projection as the event
/// cursor, never an internal Run-record tag that can become visible before its
/// control owner is released.
fn boundary_status_token(state: &RuntimeEventCursorState) -> String {
    match state {
        RuntimeEventCursorState::Running => "running".into(),
        RuntimeEventCursorState::Cancelling => "cancelling".into(),
        RuntimeEventCursorState::WaitingApproval => "waiting_approval".into(),
        RuntimeEventCursorState::Suspended => "suspended".into(),
        RuntimeEventCursorState::Interrupted => "interrupted".into(),
        RuntimeEventCursorState::Terminal { status }
        | RuntimeEventCursorState::Retired { status, .. } => status_token(*status),
    }
}
