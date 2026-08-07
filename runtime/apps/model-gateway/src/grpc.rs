use crate::{
    ModelInvocationDecodeError, ModelPolicyRouteResolver, ProviderAdapter, ProviderCredential,
    ProviderExecutionError, ProviderRoute, decode_model_invocation, execute_with_safe_failover,
};
use agent_model_gateway_protocol::v1::model_event::Body as EventBody;
use agent_model_gateway_protocol::v1::model_execution_server::ModelExecution;
use agent_model_gateway_protocol::v1::{
    Completed, Failed, FinishReason, ModelErrorKind as WireErrorKind, ModelEvent, ModelInvocation,
    TextDelta, ToolCall as WireToolCall, Usage,
};
use agent_protocol::{ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent};
use agent_workload_identity::{
    RequiredCapability, WorkloadIdentityBinding, WorkloadIdentityClaims, WorkloadTokenVerifier,
};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct ModelExecutionGrpcService {
    routes: Vec<ProviderRoute>,
    route_resolver: Option<ModelPolicyRouteResolver>,
    verifier: WorkloadTokenVerifier,
}

struct CancellationStream {
    inner: ReceiverStream<Result<ModelEvent, Status>>,
    cancellation: CancellationToken,
}

impl Stream for CancellationStream {
    type Item = Result<ModelEvent, Status>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl Drop for CancellationStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl ModelExecutionGrpcService {
    #[must_use]
    pub fn new(
        adapter: impl Into<ProviderAdapter>,
        credential: ProviderCredential,
        verifier: WorkloadTokenVerifier,
    ) -> Self {
        Self {
            routes: vec![ProviderRoute::new("default", adapter, credential)],
            route_resolver: None,
            verifier,
        }
    }

    pub fn with_routes(
        routes: Vec<ProviderRoute>,
        verifier: WorkloadTokenVerifier,
    ) -> Result<Self, ProviderExecutionError> {
        if routes.is_empty() {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "model gateway requires at least one provider route".into(),
            ));
        }
        Ok(Self {
            routes,
            route_resolver: None,
            verifier,
        })
    }

    #[must_use]
    pub fn with_route_resolver(mut self, route_resolver: ModelPolicyRouteResolver) -> Self {
        self.route_resolver = Some(route_resolver);
        self
    }
}

#[tonic::async_trait]
impl ModelExecution for ModelExecutionGrpcService {
    type ExecuteStream =
        Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send + Sync + 'static>>;

    async fn execute(
        &self,
        request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
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
                RequiredCapability::new("model-gateway", "model.execute", true),
                Utc::now().timestamp_millis(),
            )
            .map_err(|_| Status::unauthenticated("invalid workload token"))?;
        let invocation = request.into_inner();
        validate_invocation_identity(&invocation, &claims)?;
        let model_request = decode_invocation(&invocation)?;
        let routes = if invocation.schema_version == 3 {
            self.route_resolver
                .as_ref()
                .ok_or_else(|| {
                    Status::failed_precondition("dynamic provider routing is not configured")
                })?
                .resolve(claims.tenant_id, &invocation.model_policy_snapshot_json)
                .map_err(|error| Status::failed_precondition(error.to_string()))?
        } else {
            self.routes.clone()
        };
        let cancellation = CancellationToken::new();
        let stream_cancellation = cancellation.clone();
        let (provider_tx, mut provider_rx) = mpsc::channel(32);
        let (grpc_tx, grpc_rx) = mpsc::channel(32);

        tokio::spawn(async move {
            let provider_cancellation = cancellation.clone();
            let provider_task = tokio::spawn(async move {
                execute_with_safe_failover(
                    &routes,
                    &model_request,
                    provider_cancellation,
                    provider_tx,
                )
                .await
                .map(|_| ())
            });
            let mut sequence = 0;
            while let Some(event) = provider_rx.recv().await {
                sequence += 1;
                if grpc_tx.send(encode_event(sequence, event)).await.is_err() {
                    cancellation.cancel();
                    break;
                }
            }
            if let Ok(Err(error)) = provider_task.await
                && !matches!(error, ProviderExecutionError::Cancelled)
            {
                sequence += 1;
                let _ = grpc_tx.send(encode_provider_failure(sequence, error)).await;
            }
        });

        Ok(Response::new(Box::pin(CancellationStream {
            inner: ReceiverStream::new(grpc_rx),
            cancellation: stream_cancellation,
        })))
    }
}

fn validate_invocation_identity(
    invocation: &ModelInvocation,
    claims: &WorkloadIdentityClaims,
) -> Result<(), Status> {
    let binding = WorkloadIdentityBinding {
        tenant_id: invocation.tenant_id.parse().unwrap_or_default(),
        run_id: invocation.run_id.parse().unwrap_or_default(),
        attempt_id: invocation.attempt_id.parse().unwrap_or_default(),
        worker_id: invocation.worker_id.parse().unwrap_or_default(),
        worker_incarnation_id: invocation.worker_incarnation_id.parse().unwrap_or_default(),
    };
    let policy_binding_matches = match invocation.schema_version {
        2 => {
            claims.schema_version == 2
                && claims.model_policy_digest.is_empty()
                && invocation.model_policy_digest.is_empty()
                && invocation.model_policy_snapshot_json.is_empty()
        }
        3 => {
            claims.schema_version == 3
                && !invocation.model_policy_snapshot_json.is_empty()
                && invocation.model_policy_digest == claims.model_policy_digest
                && invocation.model_policy_digest
                    == hex::encode(Sha256::digest(&invocation.model_policy_snapshot_json))
        }
        _ => false,
    };
    let matches = policy_binding_matches
        && claims.authorizes(&binding)
        && invocation.model_policy_id == claims.model_policy_id.to_string()
        && invocation.expires_at_unix_ms <= claims.expires_at_unix_ms
        && invocation.expires_at_unix_ms > Utc::now().timestamp_millis();
    if matches {
        Ok(())
    } else {
        Err(Status::permission_denied(
            "workload identity does not authorize this model invocation",
        ))
    }
}

fn decode_invocation(invocation: &ModelInvocation) -> Result<ModelRequest, Status> {
    // The decoder itself is transport neutral so the embedded host and this
    // gRPC stage cannot drift; only the Status mapping is gRPC specific.
    decode_model_invocation(invocation).map_err(|error| match error {
        ModelInvocationDecodeError::UnsupportedContentPart => {
            Status::unimplemented("this gRPC transport stage does not support this content type")
        }
        ModelInvocationDecodeError::StructuredOutputUnsupported => {
            Status::unimplemented("structured-output transport mapping is not implemented yet")
        }
        other => Status::invalid_argument(other.to_string()),
    })
}

fn encode_event(sequence: u64, event: ModelStreamEvent) -> Result<ModelEvent, Status> {
    let body = match event {
        ModelStreamEvent::TextDelta { text } => EventBody::TextDelta(TextDelta { text }),
        ModelStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cost_micros,
        } => EventBody::Usage(Usage {
            input_tokens,
            output_tokens,
            cost_micros,
        }),
        ModelStreamEvent::Completed { reason } => EventBody::Completed(Completed {
            reason: encode_finish_reason(reason) as i32,
        }),
        ModelStreamEvent::Failed {
            kind,
            retryable,
            message,
        } => EventBody::Failed(Failed {
            kind: encode_error_kind(kind) as i32,
            retryable,
            message,
        }),
        ModelStreamEvent::ToolCall {
            id,
            name,
            arguments,
        } => EventBody::ToolCall(WireToolCall {
            id,
            name,
            arguments_json: serde_json::to_vec(&arguments)
                .map_err(|_| Status::internal("tool-call arguments could not be encoded"))?,
        }),
    };
    Ok(ModelEvent {
        schema_version: 1,
        sequence,
        body: Some(body),
    })
}

fn encode_provider_failure(
    sequence: u64,
    error: ProviderExecutionError,
) -> Result<ModelEvent, Status> {
    match error {
        ProviderExecutionError::Provider {
            kind,
            retryable,
            message,
            ..
        } => encode_event(
            sequence,
            ModelStreamEvent::Failed {
                kind,
                retryable,
                message,
            },
        ),
        ProviderExecutionError::InvalidConfiguration(message) => {
            Err(Status::failed_precondition(message))
        }
        ProviderExecutionError::ConsumerClosed => {
            Err(Status::cancelled("provider consumer closed"))
        }
        ProviderExecutionError::Cancelled => Err(Status::cancelled("provider execution cancelled")),
    }
}

const fn encode_finish_reason(reason: ModelFinishReason) -> FinishReason {
    match reason {
        ModelFinishReason::Stop => FinishReason::Stop,
        ModelFinishReason::ToolCalls => FinishReason::ToolCalls,
        ModelFinishReason::Length => FinishReason::Length,
        ModelFinishReason::ContentFilter => FinishReason::ContentFilter,
    }
}

const fn encode_error_kind(kind: ModelErrorKind) -> WireErrorKind {
    match kind {
        ModelErrorKind::Authentication => WireErrorKind::Authentication,
        ModelErrorKind::Billing => WireErrorKind::Billing,
        ModelErrorKind::RateLimited => WireErrorKind::RateLimited,
        ModelErrorKind::Timeout => WireErrorKind::Timeout,
        ModelErrorKind::Protocol => WireErrorKind::Protocol,
        ModelErrorKind::ContextOverflow => WireErrorKind::ContextOverflow,
        ModelErrorKind::CapabilityMismatch => WireErrorKind::CapabilityMismatch,
        ModelErrorKind::Unavailable => WireErrorKind::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use uuid::Uuid;

    fn bound_invocation_and_claims() -> (ModelInvocation, WorkloadIdentityClaims) {
        let tenant_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let worker_id = Uuid::now_v7();
        let policy_id = Uuid::now_v7();
        let snapshot =
            br#"{"schema_version":1,"routing":"single_provider","candidates":[]}"#.to_vec();
        let digest = hex::encode(Sha256::digest(&snapshot));
        let now = Utc::now().timestamp_millis();
        let invocation = ModelInvocation {
            schema_version: 3,
            tenant_id: tenant_id.to_string(),
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_string(),
            worker_id: worker_id.to_string(),
            worker_incarnation_id: worker_id.to_string(),
            model_policy_id: policy_id.to_string(),
            expires_at_unix_ms: now + 60_000,
            model_policy_snapshot_json: snapshot,
            model_policy_digest: digest.clone(),
            ..Default::default()
        };
        let claims = WorkloadIdentityClaims {
            schema_version: 3,
            tenant_id,
            run_id,
            attempt_id,
            worker_id,
            worker_incarnation_id: worker_id,
            model_policy_id: policy_id,
            model_policy_digest: digest,
            audiences: BTreeSet::from(["model-gateway".into()]),
            scopes: BTreeSet::from(["model.execute".into()]),
            issued_at_unix_ms: now,
            expires_at_unix_ms: now + 60_000,
        };
        (invocation, claims)
    }

    #[test]
    fn schema_three_binds_the_exact_policy_snapshot_to_the_workload_identity() {
        let (invocation, claims) = bound_invocation_and_claims();

        validate_invocation_identity(&invocation, &claims).unwrap();
    }

    #[test]
    fn schema_three_rejects_worker_snapshot_tampering() {
        let (mut invocation, claims) = bound_invocation_and_claims();
        invocation.model_policy_snapshot_json.push(b' ');

        assert_eq!(
            validate_invocation_identity(&invocation, &claims)
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
    }
}
