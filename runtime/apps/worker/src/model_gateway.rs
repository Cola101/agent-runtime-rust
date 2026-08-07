use agent_model_gateway_protocol::v1::model_event::Body;
use agent_model_gateway_protocol::v1::model_execution_client::ModelExecutionClient;
use agent_model_gateway_protocol::v1::{
    FinishReason, ModelErrorKind as WireErrorKind, ModelEvent, ModelInvocation,
};
use agent_protocol::{ModelErrorKind, ModelFinishReason, ModelStreamEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};

#[derive(Clone)]
pub struct GrpcModelGatewayClient {
    inner: ModelExecutionClient<Channel>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelGatewayClientError {
    #[error("model gateway transport failed: {0}")]
    Transport(String),
    #[error("model gateway RPC failed with {code}: {message}")]
    Rpc { code: Code, message: String },
    #[error("model gateway request was cancelled")]
    Cancelled,
    #[error("model gateway event consumer closed")]
    ConsumerClosed,
    #[error("model gateway returned an invalid event: {0}")]
    InvalidEvent(String),
}

impl GrpcModelGatewayClient {
    pub async fn connect(endpoint: String) -> Result<Self, ModelGatewayClientError> {
        let inner = ModelExecutionClient::connect(endpoint)
            .await
            .map_err(|error| ModelGatewayClientError::Transport(error.to_string()))?;
        Ok(Self { inner })
    }

    pub async fn connect_with_mtls(
        endpoint: String,
        materials: ClientMtlsMaterials,
    ) -> Result<Self, ModelGatewayClientError> {
        let endpoint = Endpoint::from_shared(endpoint)
            .map_err(|error| ModelGatewayClientError::Transport(error.to_string()))?
            .tls_config(materials.into_tonic())
            .map_err(|error| ModelGatewayClientError::Transport(error.to_string()))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| ModelGatewayClientError::Transport(error.to_string()))?;
        Ok(Self {
            inner: ModelExecutionClient::new(channel),
        })
    }

    pub async fn execute(
        &mut self,
        invocation: ModelInvocation,
        workload_token: &str,
        cancellation: CancellationToken,
        events: mpsc::Sender<ModelStreamEvent>,
    ) -> Result<(), ModelGatewayClientError> {
        let authorization = MetadataValue::try_from(format!("Bearer {workload_token}"))
            .map_err(|_| ModelGatewayClientError::Transport("invalid workload token".into()))?;
        let mut request = tonic::Request::new(invocation);
        request
            .metadata_mut()
            .insert("authorization", authorization);
        let response = self.inner.execute(request).await.map_err(rpc_error)?;
        let mut stream = response.into_inner();
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => return Err(ModelGatewayClientError::Cancelled),
                event = stream.message() => event.map_err(rpc_error)?,
            };
            let Some(event) = event else {
                return Err(ModelGatewayClientError::InvalidEvent(
                    "model gateway stream ended without a terminal event".into(),
                ));
            };
            let event = decode_event(event)?;
            let terminal = matches!(
                event,
                ModelStreamEvent::Completed { .. } | ModelStreamEvent::Failed { .. }
            );
            events
                .send(event)
                .await
                .map_err(|_| ModelGatewayClientError::ConsumerClosed)?;
            if terminal {
                return Ok(());
            }
        }
    }
}

fn rpc_error(status: tonic::Status) -> ModelGatewayClientError {
    ModelGatewayClientError::Rpc {
        code: status.code(),
        message: status.message().to_owned(),
    }
}

fn decode_event(event: ModelEvent) -> Result<ModelStreamEvent, ModelGatewayClientError> {
    if event.schema_version != 1 || event.sequence == 0 {
        return Err(ModelGatewayClientError::InvalidEvent(
            "unsupported schema version or zero sequence".into(),
        ));
    }
    match event.body {
        Some(Body::TextDelta(delta)) => Ok(ModelStreamEvent::TextDelta { text: delta.text }),
        Some(Body::Usage(usage)) => Ok(ModelStreamEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost_micros: usage.cost_micros,
        }),
        Some(Body::Completed(completed)) => Ok(ModelStreamEvent::Completed {
            reason: decode_finish_reason(completed.reason)?,
        }),
        Some(Body::Failed(failed)) => Ok(ModelStreamEvent::Failed {
            kind: decode_error_kind(failed.kind)?,
            retryable: failed.retryable,
            message: failed.message,
        }),
        Some(Body::ToolCall(tool_call)) => Ok(ModelStreamEvent::ToolCall {
            id: tool_call.id,
            name: tool_call.name,
            arguments: serde_json::from_slice(&tool_call.arguments_json).map_err(|_| {
                ModelGatewayClientError::InvalidEvent(
                    "tool-call arguments are not valid JSON".into(),
                )
            })?,
        }),
        None => Err(ModelGatewayClientError::InvalidEvent(
            "event body is missing".into(),
        )),
    }
}

fn decode_finish_reason(value: i32) -> Result<ModelFinishReason, ModelGatewayClientError> {
    match FinishReason::try_from(value) {
        Ok(FinishReason::Stop) => Ok(ModelFinishReason::Stop),
        Ok(FinishReason::ToolCalls) => Ok(ModelFinishReason::ToolCalls),
        Ok(FinishReason::Length) => Ok(ModelFinishReason::Length),
        Ok(FinishReason::ContentFilter) => Ok(ModelFinishReason::ContentFilter),
        _ => Err(ModelGatewayClientError::InvalidEvent(
            "finish reason is unspecified".into(),
        )),
    }
}

fn decode_error_kind(value: i32) -> Result<ModelErrorKind, ModelGatewayClientError> {
    match WireErrorKind::try_from(value) {
        Ok(WireErrorKind::Authentication) => Ok(ModelErrorKind::Authentication),
        Ok(WireErrorKind::Billing) => Ok(ModelErrorKind::Billing),
        Ok(WireErrorKind::RateLimited) => Ok(ModelErrorKind::RateLimited),
        Ok(WireErrorKind::Timeout) => Ok(ModelErrorKind::Timeout),
        Ok(WireErrorKind::Protocol) => Ok(ModelErrorKind::Protocol),
        Ok(WireErrorKind::ContextOverflow) => Ok(ModelErrorKind::ContextOverflow),
        Ok(WireErrorKind::CapabilityMismatch) => Ok(ModelErrorKind::CapabilityMismatch),
        Ok(WireErrorKind::Unavailable) => Ok(ModelErrorKind::Unavailable),
        _ => Err(ModelGatewayClientError::InvalidEvent(
            "model error kind is unspecified".into(),
        )),
    }
}
use agent_grpc_security::ClientMtlsMaterials;
