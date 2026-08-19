use agent_grpc_security::{ClientMtlsMaterials, ServerMtlsMaterials};
use agent_model_gateway::{
    ModelExecutionGrpcService, OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential,
    ProviderPricing, WorkloadIdentityClaims, WorkloadTokenVerifier,
};
use agent_model_gateway_protocol::v1::model_execution_server::ModelExecutionServer;
use agent_model_gateway_protocol::v1::{
    Completed, ContentPart, FinishReason, ModelEvent, ModelInvocation, ModelMessage, ModelRole,
    ModelTool, PrivateStateOmitted, ProviderPrivateState as WireProviderPrivateState, Reasoning,
    ReasoningPolicy, Refusal, TextDelta, TextPart, content_part, model_event,
};
use agent_protocol::{
    ModelErrorKind, ModelFinishReason, ModelStreamEvent, Placement, ProviderPrivateState,
    RunExecutionCommand,
};
use agent_runtime_worker::{
    GrpcModelGatewayClient, ModelExecutionSupervisor, ModelExecutionUpdate,
    ModelGatewayClientError, WorkerProcessor,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{TimeZone, Utc};
use ed25519_dalek::{Signer, SigningKey};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use std::collections::BTreeSet;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::{Code, Request, Response, Status};
use uuid::Uuid;

const TEST_SIGNING_KEY: [u8; 32] = [7; 32];
const EXECUTION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v1.example.json");

#[tokio::test]
async fn authenticated_worker_streams_real_provider_events_without_receiving_provider_credentials()
{
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let (provider_endpoint, provider_request, provider_server) = spawn_provider(sse).await;
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway(provider_endpoint).await;
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let prepared = processor
        .prepare_model_invocation(command.attempt_id)
        .unwrap();
    let mut client = GrpcModelGatewayClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    client
        .execute(
            prepared.invocation,
            prepared.workload_token.as_str(),
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    let provider_request = provider_request.await.unwrap();

    assert!(
        provider_request
            .to_ascii_lowercase()
            .contains("authorization: bearer provider-only-secret")
    );
    assert!(!token.contains("provider-only-secret"));
    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta {
                text: "hello".into(),
                // The choice index this fixture does send, carried the whole way
                // across gRPC. I assumed it sent none; the gate said otherwise,
                // which is the argument for asserting the value rather than the
                // presence of a field.
                block: Some(0),
            },
            ModelStreamEvent::Usage {
                input_tokens: 4,
                output_tokens: 1,
                cost_micros: 6,
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        ]
    );

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
    provider_server.await.unwrap();
}

#[tokio::test]
async fn supervisor_forwards_one_dispatch_model_stream_in_order_and_only_once() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let (provider_endpoint, _provider_request, provider_server) = spawn_provider(sse).await;
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway(provider_endpoint).await;
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let prepared = processor
        .prepare_model_invocation(command.attempt_id)
        .unwrap();
    let cancellation = processor.cancellation_token(command.attempt_id).unwrap();
    let client = GrpcModelGatewayClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let mut supervisor = ModelExecutionSupervisor::new(8);

    assert!(supervisor.start(command.attempt_id, client.clone(), prepared, cancellation));
    assert!(
        !supervisor.start(
            command.attempt_id,
            client,
            processor
                .prepare_model_invocation(command.attempt_id)
                .unwrap(),
            processor.cancellation_token(command.attempt_id).unwrap(),
        )
    );

    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            attempt_id,
            event: ModelStreamEvent::TextDelta { ref text, .. },
        }) if attempt_id == command.attempt_id && text == "hello"
    ));
    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            attempt_id,
            event: ModelStreamEvent::Usage {
                input_tokens: 4,
                output_tokens: 1,
                cost_micros: 6,
            },
        }) if attempt_id == command.attempt_id
    ));
    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            attempt_id,
            event: ModelStreamEvent::Completed { reason: ModelFinishReason::Stop },
        }) if attempt_id == command.attempt_id
    ));
    assert_eq!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Finished {
            attempt_id: command.attempt_id,
        })
    );

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
    provider_server.await.unwrap();
}

#[tokio::test]
async fn completed_model_turn_releases_the_attempt_for_a_later_tool_result_turn() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let (provider_one, _request_one, provider_server_one) = spawn_provider(sse).await;
    let (gateway_one, shutdown_one, gateway_server_one) = spawn_gateway(provider_one).await;
    let (provider_two, _request_two, provider_server_two) = spawn_provider(sse).await;
    let (gateway_two, shutdown_two, gateway_server_two) = spawn_gateway(provider_two).await;
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let mut supervisor = ModelExecutionSupervisor::new(8);

    assert!(
        supervisor.start(
            command.attempt_id,
            GrpcModelGatewayClient::connect(gateway_one).await.unwrap(),
            processor
                .prepare_model_invocation(command.attempt_id)
                .unwrap(),
            processor.cancellation_token(command.attempt_id).unwrap(),
        )
    );
    for _ in 0..4 {
        supervisor.recv(Duration::from_secs(1)).await.unwrap();
    }

    assert!(
        supervisor.start(
            command.attempt_id,
            GrpcModelGatewayClient::connect(gateway_two).await.unwrap(),
            processor
                .prepare_model_invocation(command.attempt_id)
                .unwrap(),
            processor.cancellation_token(command.attempt_id).unwrap(),
        )
    );
    for _ in 0..4 {
        supervisor.recv(Duration::from_secs(1)).await.unwrap();
    }

    shutdown_one.send(()).ok();
    shutdown_two.send(()).ok();
    gateway_server_one.await.unwrap();
    gateway_server_two.await.unwrap();
    provider_server_one.await.unwrap();
    provider_server_two.await.unwrap();
}

#[tokio::test]
async fn supervisor_cancellation_stops_inflight_provider_without_emitting_failure() {
    let first_delta = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"started\"},\"finish_reason\":null}]}\n\n";
    let (provider_endpoint, provider_disconnected, provider_server) =
        spawn_hanging_provider(first_delta).await;
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway(provider_endpoint).await;
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let mut supervisor = ModelExecutionSupervisor::new(8);
    supervisor.start(
        command.attempt_id,
        GrpcModelGatewayClient::connect(gateway_endpoint)
            .await
            .unwrap(),
        processor
            .prepare_model_invocation(command.attempt_id)
            .unwrap(),
        processor.cancellation_token(command.attempt_id).unwrap(),
    );
    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            event: ModelStreamEvent::TextDelta { ref text, .. },
            ..
        }) if text == "started"
    ));

    processor.cancel(command.attempt_id).unwrap();

    assert_eq!(
        supervisor.recv(Duration::from_millis(500)).await,
        Some(ModelExecutionUpdate::Cancelled {
            attempt_id: command.attempt_id,
        })
    );
    tokio::time::timeout(Duration::from_millis(500), provider_disconnected)
        .await
        .expect("supervisor cancellation must close provider HTTP")
        .unwrap();

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
    provider_server.await.unwrap();
}

#[tokio::test]
async fn workload_token_for_another_tenant_is_rejected_before_provider_egress() {
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway("http://127.0.0.1:1/v1/chat/completions".into()).await;
    let claims = claims();
    let mut invocation = invocation(&claims);
    invocation.tenant_id = Uuid::now_v7().to_string();
    let token = sign_token(&claims);
    let mut client = GrpcModelGatewayClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let (events_tx, _events_rx) = mpsc::channel(1);

    let error = client
        .execute(invocation, &token, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ModelGatewayClientError::Rpc {
            code: Code::PermissionDenied,
            ..
        }
    ));

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn workload_token_for_another_worker_incarnation_is_rejected_before_provider_egress() {
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway("http://127.0.0.1:1/v1/chat/completions".into()).await;
    let claims = claims();
    let mut invocation = invocation(&claims);
    invocation.worker_incarnation_id = Uuid::now_v7().to_string();
    let token = sign_token(&claims);
    let mut client = GrpcModelGatewayClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let (events_tx, _events_rx) = mpsc::channel(1);

    let error = client
        .execute(invocation, &token, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ModelGatewayClientError::Rpc {
            code: Code::PermissionDenied,
            ..
        }
    ));
    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
}

#[tokio::test]
async fn worker_cancellation_crosses_grpc_and_closes_the_provider_http_stream() {
    let first_delta = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"started\"},\"finish_reason\":null}]}\n\n";
    let (provider_endpoint, provider_disconnected, provider_server) =
        spawn_hanging_provider(first_delta).await;
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway(provider_endpoint).await;
    let claims = claims();
    let invocation = invocation(&claims);
    let token = sign_token(&claims);
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.tenant_id = claims.tenant_id;
    command.run_id = claims.run_id;
    command.attempt_id = claims.attempt_id;
    command.worker_id = claims.worker_id;
    command.worker_incarnation_id = claims.worker_incarnation_id;
    command.issued_at = Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::seconds(30);
    let mut processor =
        WorkerProcessor::new(command.worker_id, vec![Placement::Cloud], 1, "0.1.0".into()).unwrap();
    processor
        .accept(
            command.clone(),
            command.issued_at + chrono::Duration::seconds(1),
        )
        .unwrap();
    processor.start(command.attempt_id).unwrap();
    let cancellation = processor.cancellation_token(command.attempt_id).unwrap();
    let task_cancellation = cancellation.clone();
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let client_task = tokio::spawn(async move {
        let mut client = GrpcModelGatewayClient::connect(gateway_endpoint)
            .await
            .unwrap();
        client
            .execute(invocation, &token, task_cancellation, events_tx)
            .await
    });

    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "started".into(),
            block: Some(0),
        })
    );
    processor.cancel(command.attempt_id).unwrap();
    let result = tokio::time::timeout(Duration::from_millis(500), client_task)
        .await
        .expect("worker cancellation must stop the gRPC stream promptly")
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), provider_disconnected)
        .await
        .expect("gRPC cancellation must close the provider HTTP connection")
        .unwrap();

    assert!(matches!(result, Err(ModelGatewayClientError::Cancelled)));

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
    provider_server.await.unwrap();
}

/// Only the arguments are streamed in fragments.
///
/// This used to send the *name* in two pieces and assert they were joined. No
/// reference implementation does that: both read the identity fields as
/// last-value-wins and append only the arguments --
/// `opencode/packages/llm/src/protocols/utils/tool-stream.ts:125-132` and
/// `openclaw/packages/ai/src/transports/openai-completions-transport.ts:771-778`.
/// The premise was not idle: it forced a concatenating assembly, which turns a
/// provider that resends its id and name on every fragment into a call named
/// `read_fileread_file`.
#[tokio::test]
async fn tool_call_survives_the_provider_and_grpc_stream_boundaries() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_7\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"notes.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (provider_endpoint, _provider_request, provider_server) = spawn_provider(sse).await;
    let (gateway_endpoint, gateway_shutdown, gateway_server) =
        spawn_gateway(provider_endpoint).await;
    let claims = claims();
    let mut invocation = invocation(&claims);
    invocation.tools = vec![ModelTool {
        name: "read_file".into(),
        description: "Read a workspace file".into(),
        input_schema_json: serde_json::to_vec(&serde_json::json!({
            "type": "object",
            "required": ["path"]
        }))
        .unwrap(),
    }];
    let token = sign_token(&claims);
    let mut client = GrpcModelGatewayClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    client
        .execute(invocation, &token, CancellationToken::new(), events_tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::ToolCall {
                id: "call_7".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"notes.md"}),
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        ]
    );

    gateway_shutdown.send(()).ok();
    gateway_server.await.unwrap();
    provider_server.await.unwrap();
}

#[tokio::test]
async fn grpc_stream_that_ends_without_a_terminal_event_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(EarlyCloseService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let claims = claims();
    let invocation = invocation(&claims);
    let token = sign_token(&claims);
    let mut client = GrpcModelGatewayClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    let error = client
        .execute(invocation, &token, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();

    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "partial".into(),
            block: None,
        })
    );
    assert!(matches!(
        error,
        ModelGatewayClientError::InvalidEvent(message)
            if message == "model gateway stream ended without a terminal event"
    ));

    shutdown_tx.send(()).ok();
    server.await.unwrap();
}

#[tokio::test]
async fn grpc_transport_preserves_rich_items_and_audit_observations() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(RichItemService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let claims = claims();
    let mut client = GrpcModelGatewayClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    client
        .execute(
            invocation(&claims),
            &sign_token(&claims),
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::Reasoning {
                summary: vec!["Checked transport.".into()],
                private_state: Some(ProviderPrivateState {
                    provider_id: "openai-primary".into(),
                    protocol: "openai_responses".into(),
                    model: "gpt-agent".into(),
                    format: "openai.responses.reasoning.v1".into(),
                    data: "opaque-state".into(),
                }),
            },
            ModelStreamEvent::Refusal {
                text: "typed refusal".into(),
            },
            ModelStreamEvent::PrivateStateOmitted {
                origin_provider_id: "origin".into(),
                target_provider_id: "fallback".into(),
                format: "private.v1".into(),
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        ]
    );
    shutdown_tx.send(()).ok();
    server.await.unwrap();
}

#[tokio::test]
async fn model_gateway_client_presents_its_identity_over_mtls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (server_tls, client_tls) = model_gateway_test_pki();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(server_tls.into_tonic())
            .unwrap()
            .add_service(ModelExecutionServer::new(EarlyCloseService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let claims = claims();
    let (events_tx, _events_rx) = mpsc::channel(8);
    let mut client =
        GrpcModelGatewayClient::connect_with_mtls(format!("https://{address}"), client_tls)
            .await
            .unwrap();

    let error = client
        .execute(
            invocation(&claims),
            &sign_token(&claims),
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ModelGatewayClientError::InvalidEvent(_)));
    shutdown_tx.send(()).ok();
    server.await.unwrap();
}

#[tokio::test]
async fn supervisor_waits_for_a_new_identity_after_unauthenticated_instead_of_failing_the_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(UnauthenticatedService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let mut supervisor = ModelExecutionSupervisor::new(8);
    supervisor.start(
        command.attempt_id,
        GrpcModelGatewayClient::connect(format!("http://{address}"))
            .await
            .unwrap(),
        processor
            .prepare_model_invocation(command.attempt_id)
            .unwrap(),
        processor.cancellation_token(command.attempt_id).unwrap(),
    );

    assert_eq!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::AuthenticationRequired {
            attempt_id: command.attempt_id,
        })
    );
    assert_eq!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Finished {
            attempt_id: command.attempt_id,
        })
    );

    shutdown_tx.send(()).ok();
    server.await.unwrap();
}

fn model_gateway_test_pki() -> (ServerMtlsMaterials, ClientMtlsMaterials) {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let server_key = KeyPair::generate().unwrap();
    let mut server_params = CertificateParams::new(vec!["model-gateway.test".into()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut client_params = CertificateParams::new(vec!["runtime-worker.test".into()]).unwrap();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();
    let ca_pem = ca_cert.pem().into_bytes();
    (
        ServerMtlsMaterials::new(
            server_cert.pem().into_bytes(),
            server_key.serialize_pem().into_bytes(),
            ca_pem.clone(),
        )
        .unwrap(),
        ClientMtlsMaterials::new(
            client_cert.pem().into_bytes(),
            client_key.serialize_pem().into_bytes(),
            ca_pem,
            "model-gateway.test".into(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn supervisor_turns_premature_gateway_close_into_non_retryable_protocol_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(EarlyCloseService))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let claims = claims();
    let token = sign_token(&claims);
    let command = execution_command(&claims, &token);
    let mut processor =
        WorkerProcessor::new(claims.worker_id, vec![Placement::Cloud], 1, "test".into()).unwrap();
    processor.accept(command.clone(), Utc::now()).unwrap();
    let mut supervisor = ModelExecutionSupervisor::new(8);
    supervisor.start(
        command.attempt_id,
        GrpcModelGatewayClient::connect(format!("http://{address}"))
            .await
            .unwrap(),
        processor
            .prepare_model_invocation(command.attempt_id)
            .unwrap(),
        processor.cancellation_token(command.attempt_id).unwrap(),
    );

    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            event: ModelStreamEvent::TextDelta { ref text, .. },
            ..
        }) if text == "partial"
    ));
    assert!(matches!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Event {
            attempt_id,
            event: ModelStreamEvent::Failed {
                kind: ModelErrorKind::Protocol,
                retryable: false,
                ..
            },
        }) if attempt_id == command.attempt_id
    ));
    assert_eq!(
        supervisor.recv(Duration::from_secs(1)).await,
        Some(ModelExecutionUpdate::Finished {
            attempt_id: command.attempt_id,
        })
    );

    shutdown_tx.send(()).ok();
    server.await.unwrap();
}

#[derive(Clone, Copy)]
struct EarlyCloseService;

#[derive(Clone, Copy)]
struct RichItemService;

#[derive(Clone, Copy)]
struct UnauthenticatedService;

#[tonic::async_trait]
impl agent_model_gateway_protocol::v1::model_execution_server::ModelExecution
    for UnauthenticatedService
{
    type ExecuteStream =
        Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send + Sync + 'static>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        Err(Status::unauthenticated("workload identity expired"))
    }
}

#[tonic::async_trait]
impl agent_model_gateway_protocol::v1::model_execution_server::ModelExecution
    for EarlyCloseService
{
    type ExecuteStream =
        Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send + Sync + 'static>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let stream = tokio_stream::iter(vec![Ok(ModelEvent {
            schema_version: 1,
            sequence: 1,
            body: Some(model_event::Body::TextDelta(TextDelta {
                text: "partial".into(),
                block: None,
            })),
        })]);
        Ok(Response::new(Box::pin(stream)))
    }
}

#[tonic::async_trait]
impl agent_model_gateway_protocol::v1::model_execution_server::ModelExecution for RichItemService {
    type ExecuteStream =
        Pin<Box<dyn Stream<Item = Result<ModelEvent, Status>> + Send + Sync + 'static>>;

    async fn execute(
        &self,
        _request: Request<ModelInvocation>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let events = vec![
            model_event::Body::Reasoning(Reasoning {
                summary: vec!["Checked transport.".into()],
                private_state: Some(WireProviderPrivateState {
                    provider_id: "openai-primary".into(),
                    protocol: "openai_responses".into(),
                    model: "gpt-agent".into(),
                    format: "openai.responses.reasoning.v1".into(),
                    data: "opaque-state".into(),
                }),
            }),
            model_event::Body::Refusal(Refusal {
                text: "typed refusal".into(),
            }),
            model_event::Body::PrivateStateOmitted(PrivateStateOmitted {
                origin_provider_id: "origin".into(),
                target_provider_id: "fallback".into(),
                format: "private.v1".into(),
            }),
            model_event::Body::Completed(Completed {
                reason: FinishReason::Stop as i32,
            }),
        ];
        let stream = tokio_stream::iter(events.into_iter().enumerate().map(|(index, body)| {
            Ok(ModelEvent {
                schema_version: 1,
                sequence: u64::try_from(index + 1).unwrap(),
                body: Some(body),
            })
        }));
        Ok(Response::new(Box::pin(stream)))
    }
}

fn claims() -> WorkloadIdentityClaims {
    let now = Utc::now().timestamp_millis();
    let worker_id = Uuid::now_v7();
    WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        worker_id,
        worker_incarnation_id: worker_id,
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["model-gateway".into(), "checkpoint-gateway".into()]),
        scopes: BTreeSet::from([
            "model.execute".into(),
            "checkpoint.read".into(),
            "checkpoint.write".into(),
        ]),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    }
}

fn invocation(claims: &WorkloadIdentityClaims) -> ModelInvocation {
    ModelInvocation {
        schema_version: 2,
        tenant_id: claims.tenant_id.to_string(),
        application_id: String::new(),
        workload_identity_id: String::new(),
        run_id: claims.run_id.to_string(),
        session_id: Uuid::now_v7().to_string(),
        workspace_id: String::new(),
        agent_version_id: String::new(),
        attempt_id: claims.attempt_id.to_string(),
        worker_id: claims.worker_id.to_string(),
        model_policy_id: claims.model_policy_id.to_string(),
        expires_at_unix_ms: claims.expires_at_unix_ms,
        messages: vec![ModelMessage {
            role: ModelRole::User as i32,
            content: vec![ContentPart {
                body: Some(content_part::Body::Text(TextPart {
                    text: "say hello".into(),
                })),
            }],
        }],
        tools: vec![],
        output_schema_json: vec![],
        reasoning: ReasoningPolicy::Balanced as i32,
        max_output_tokens: 64,
        worker_incarnation_id: claims.worker_incarnation_id.to_string(),
        model_policy_snapshot_json: Vec::new(),
        model_policy_digest: String::new(),
        runtime_policy_snapshot_json: Vec::new(),
        runtime_policy_digest: String::new(),
    }
}

fn execution_command(claims: &WorkloadIdentityClaims, token: &str) -> RunExecutionCommand {
    let issued_at = Utc
        .timestamp_millis_opt(claims.issued_at_unix_ms)
        .single()
        .unwrap();
    let expires_at = Utc
        .timestamp_millis_opt(claims.expires_at_unix_ms)
        .single()
        .unwrap();
    serde_json::from_value(serde_json::json!({
        "schema_version": 2,
        "message_id": Uuid::now_v7(),
        "tenant_id": claims.tenant_id,
        "run_id": claims.run_id,
        "session_id": Uuid::now_v7(),
        "workspace_id": Uuid::now_v7(),
        "agent_version_id": Uuid::now_v7(),
        "model_policy_id": claims.model_policy_id,
        "attempt_id": claims.attempt_id,
        "worker_id": claims.worker_id,
        "worker_incarnation_id": claims.worker_incarnation_id,
        "owner_epoch": 1,
        "fencing_token": Uuid::now_v7(),
        "issued_at": issued_at,
        "lease_expires_at": expires_at,
        "workload_token": token,
        "input": "say hello",
        "budget": {
            "max_tokens": 64,
            "max_cost_cents": 100,
            "max_duration_seconds": 60
        }
    }))
    .unwrap()
}

fn sign_token(claims: &WorkloadIdentityClaims) -> String {
    let signing_key = SigningKey::from_bytes(&TEST_SIGNING_KEY);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = signing_key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

async fn spawn_gateway(
    provider_endpoint: String,
) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let adapter = OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
        endpoint: provider_endpoint,
        model: "test-model".into(),
        pricing: ProviderPricing {
            input_million_tokens_micros: 1_000_000,
            output_million_tokens_micros: 2_000_000,
        },
        response_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_secs(2),
    })
    .unwrap();
    let credential = ProviderCredential::bearer("provider-only-secret").unwrap();
    let signing_key = SigningKey::from_bytes(&TEST_SIGNING_KEY);
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    let service = ModelExecutionGrpcService::new(adapter, credential, verifier);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(ModelExecutionServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });
    (format!("http://{address}"), shutdown_tx, server)
}

async fn spawn_provider(
    body: &str,
) -> (
    String,
    oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_owned();
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut socket).await;
        request_tx.send(request).ok();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        request_rx,
        server,
    )
}

async fn spawn_hanging_provider(
    first_delta: &str,
) -> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_delta = first_delta.to_owned();
    let (disconnected_tx, disconnected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_http_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(first_delta.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let mut buffer = [0_u8; 1024];
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        disconnected_tx.send(()).ok();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        disconnected_rx,
        server,
    )
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before headers completed");
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(buffer).unwrap()
}
