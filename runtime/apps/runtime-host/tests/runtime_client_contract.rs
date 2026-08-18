//! Headless desktop-integration acceptance gate.
//!
//! Setup constructs one Embedded Runtime, then the consumer receives only the
//! stable `RuntimeClient`. No gRPC server, Java control plane, GUI framework or
//! daemon is involved. A Tauri command layer can therefore embed this exact
//! path; an Electron or Java adapter can use the same contract over gRPC.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::client::{
    RUNTIME_CAPABILITY_EVENTS_WATCH, RUNTIME_CAPABILITY_RUN_CONTROL, RUNTIME_CAPABILITY_RUN_SUBMIT,
    RUNTIME_CLIENT_CONTRACT_VERSION, RUNTIME_CLIENT_SCHEMA_VERSION, RuntimeClient,
    RuntimeClientErrorCode, RuntimeClientEventCursorRequest, RuntimeClientHello,
    RuntimeSubmitRequest,
};
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
    RuntimeControlAction, RuntimeControlCommand, RuntimeEventCursorState, RuntimeEventStreamItem,
    RuntimeProfile,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const MODEL_REPLY: &str = "headless Runtime client is ready";

async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{MODEL_REPLY}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    endpoint
}

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_client_initializes_submits_and_streams_a_real_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let provider_endpoint = spawn_provider().await;
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 4,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation,
            config: LocalRuntimeConfig {
                state_root: state.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                agent_instructions: "Answer briefly.".into(),
                delegated_scopes: BTreeSet::new(),
                subagent_roles: Vec::new(),
                model_routing: LocalModelRoutingConfig {
                    allowed_regions: BTreeSet::from(["local".into()]),
                    data_class: DataClass::Internal,
                    max_cost_per_million_tokens_micros: 1_000_000,
                    health_policy: Default::default(),
                    candidates: vec![LocalProviderConfig {
                        id: "loopback".into(),
                        protocol: ProviderProtocol::OpenAiCompatible,
                        endpoint: provider_endpoint,
                        model: "test-model".into(),
                        api_key: "test-key".into(),
                        region: "local".into(),
                        accepted_data_classes: BTreeSet::from([DataClass::Internal]),
                        capabilities: BTreeSet::from([Capability::Text]),
                        healthy: true,
                        latency_ms: 1,
                        cost_per_million_tokens_micros: 1,
                        response_timeout_ms: 5_000,
                        stream_idle_timeout_ms: 5_000,
                    }],
                },
                mcp_servers: Vec::new(),
                mcp_lifecycle: LocalMcpLifecycleConfig::default(),
                trusted_workspace_tool: None,
                process_session: None,
                consent: LocalToolConsent::Ask,
                budget: RunBudget {
                    max_tokens: 1_000,
                    max_cost_cents: 100,
                    max_duration_seconds: 60,
                },
                runtime_policy: RuntimeExecutionPolicySnapshot::default(),
            },
        }],
    )
    .expect("Runtime");

    // The integration consumer receives this port, not an EmbeddedRuntime or
    // any path/credential/configuration object.
    let client_port = RuntimeClient::new(Arc::new(runtime));
    let client = client_port
        .initialize(&RuntimeClientHello {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            min_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            max_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            required_capabilities: BTreeSet::from([
                RUNTIME_CAPABILITY_RUN_SUBMIT.into(),
                RUNTIME_CAPABILITY_RUN_CONTROL.into(),
                RUNTIME_CAPABILITY_EVENTS_WATCH.into(),
            ]),
        })
        .expect("compatible client contract");
    let descriptor = client.descriptor();
    assert_eq!(descriptor.contract_version, 1);

    let oversized_run_id = Uuid::now_v7();
    let oversized = client
        .submit(RuntimeSubmitRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            run_id: oversized_run_id,
            input: "x".repeat(32_001),
        })
        .await
        .expect_err("the client edge must enforce the Kernel input bound");
    assert_eq!(oversized.code, RuntimeClientErrorCode::InvalidRequest);
    let absent = client
        .read_events(RuntimeClientEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation,
            run_id: oversized_run_id,
            after_sequence: 0,
            limit: 1,
        })
        .expect_err("rejected input must not create durable Run state");
    assert_eq!(absent.code, RuntimeClientErrorCode::NotFound);

    let oversized_control = client
        .control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation,
            run_id: Uuid::now_v7(),
            expected_owner_epoch: 1,
            action: RuntimeControlAction::Cancel {
                reason: "x".repeat(64 * 1024),
            },
        })
        .await
        .expect_err("typed clients must obey the same action bound as gRPC");
    assert_eq!(
        oversized_control.code,
        RuntimeClientErrorCode::InvalidRequest
    );

    let run_id = Uuid::now_v7();
    let receipt = client
        .submit(RuntimeSubmitRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            run_id,
            input: "prove the headless integration path".into(),
        })
        .await
        .expect("submit");
    assert_eq!(receipt.run_id, run_id);

    let mut stream = client
        .watch_events(invocation, run_id, 0, 16)
        .expect("watch");
    let mut sequences = Vec::new();
    let mut transcript = String::new();
    let terminal = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match stream
                .recv()
                .await
                .expect("stream ended")
                .expect("stream item")
            {
                RuntimeEventStreamItem::Event { event, .. } => {
                    sequences.push(event.sequence);
                    transcript.push_str(&event.payload.to_string());
                }
                RuntimeEventStreamItem::Boundary {
                    state: RuntimeEventCursorState::Terminal { status },
                    ..
                }
                | RuntimeEventStreamItem::Boundary {
                    state: RuntimeEventCursorState::Retired { status, .. },
                    ..
                } => break status,
                RuntimeEventStreamItem::Boundary { .. } => {}
            }
        }
    })
    .await
    .expect("terminal boundary");

    assert_eq!(terminal, RunStatus::Succeeded);
    assert!(transcript.contains(MODEL_REPLY));
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "event cursor must remain strictly monotonic: {sequences:?}"
    );
}
