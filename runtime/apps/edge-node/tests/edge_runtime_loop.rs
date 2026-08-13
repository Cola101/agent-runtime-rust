use agent_edge_node::{
    EdgeControlPlaneTrust, EdgeDeviceIdentity, EdgeNode, EdgeNodeStore, EdgeOutboxPayload,
    EdgeTaskReceiptStatus, VerifiedEdgeEnrollment, verify_edge_enrollment_request,
    verify_edge_task_token_for_enrollment,
};
use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    EDGE_TASK_SCHEMA_VERSION, EdgeTaskClaims, RUNTIME_INVOCATION_SCHEMA_VERSION, RunBudget,
    RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

mod common;

const KEY_ID: &str = "control-2026-08";

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: RUNTIME_INVOCATION_SCHEMA_VERSION,
        tenant_id: Uuid::from_u128(201),
        application_id: Uuid::from_u128(202),
        workload_identity_id: Uuid::from_u128(203),
        workspace_id: Uuid::from_u128(204),
        agent_version_id: Uuid::from_u128(205),
        model_policy_id: Uuid::from_u128(206),
    }
}

fn enrollment(state_root: &std::path::Path, now: i64) -> VerifiedEdgeEnrollment {
    common::verified_enrollment(
        state_root,
        Uuid::from_u128(2100),
        Uuid::from_u128(208),
        3,
        now,
    )
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Answer from the registered edge profile only.".into(),
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
                endpoint,
                model: "edge-test-model".into(),
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
    }
}

fn token(key: &SigningKey, enrollment: &VerifiedEdgeEnrollment, now: i64, run_id: Uuid) -> String {
    let enrollment_claims = enrollment.claims();
    let claims = EdgeTaskClaims {
        schema_version: EDGE_TASK_SCHEMA_VERSION,
        task_id: Uuid::from_u128(207),
        enrollment_id: enrollment_claims.enrollment_id,
        node_id: enrollment_claims.node_id,
        node_generation: enrollment_claims.node_generation,
        capability_manifest_digest: enrollment_claims.capability_manifest_digest.clone(),
        required_capabilities: BTreeSet::from(["runtime.agent.execute".into()]),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now + 60_000,
        invocation: invocation(),
        run_id,
        session_id: run_id,
        workspace_owner_epoch: 17,
        input: "identify this edge execution".into(),
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize claims"));
    let signed = format!("edge-task-v1.{KEY_ID}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    format!("{signed}.{signature}")
}

async fn spawn_provider() -> (String, tokio::task::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let mut request = vec![0_u8; 64 * 1024];
        let _ = socket.read(&mut request).await;
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"edge-ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.expect("reply");
        1
    });
    (endpoint, task)
}

fn runtime(
    runtime_state: &tempfile::TempDir,
    workspace: &tempfile::TempDir,
    endpoint: String,
) -> EmbeddedRuntime {
    EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 4,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation: invocation(),
            config: config(
                runtime_state.path().to_path_buf(),
                workspace.path().canonicalize().expect("workspace"),
                endpoint,
            ),
        }],
    )
    .expect("embedded Runtime")
}

/// The production break this catches is building an Edge protocol that verifies
/// correctly but never enters the real Agent Runtime, or redelivers a completed
/// task after node restart. It uses the real HTTP/SSE adapter and durable Host
/// event log; only the external model is a local loopback endpoint.
#[tokio::test]
async fn signed_edge_task_runs_once_and_replays_its_durable_receipt_after_restart() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_provider().await;
    let key = SigningKey::from_bytes(&[51; 32]);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
        .expect("trust");
    let run_id = Uuid::from_u128(209);
    let now = Utc::now().timestamp_millis();
    let identity = EdgeDeviceIdentity::load_or_create(edge_state.path()).expect("device identity");
    let challenge_id = Uuid::from_u128(2102);
    let challenge_nonce = [21_u8; 32];
    let enrollment_request = identity
        .create_enrollment_request(
            challenge_id,
            &challenge_nonce,
            &common::capability_manifest(),
            now,
            now + 60_000,
        )
        .expect("enrollment request");
    let verified_request =
        verify_edge_enrollment_request(&enrollment_request, challenge_id, &challenge_nonce, now)
            .expect("verified enrollment request");
    assert_eq!(verified_request.claims().device_id, identity.device_id());
    let enrollment = enrollment(edge_state.path(), now);
    assert_eq!(enrollment.claims().device_id, identity.device_id());
    let signed = token(&key, &enrollment, now, run_id);
    let node = EdgeNode::new(
        enrollment.clone(),
        trust,
        EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store"),
        runtime(&runtime_state, &workspace, endpoint.clone()),
    )
    .expect("edge node");

    let receipt = node
        .execute_task_token(&signed, now)
        .await
        .expect("edge execution");
    assert_eq!(receipt.status, EdgeTaskReceiptStatus::Succeeded);
    assert_eq!(receipt.output, "edge-ok");
    assert_eq!(receipt.node_id, Uuid::from_u128(208));
    assert_eq!(receipt.node_generation, 3);
    assert_eq!(receipt.invocation, invocation());
    assert_eq!(receipt.session_id, run_id);
    assert_eq!(receipt.workspace_owner_epoch, 17);
    assert!(receipt.last_runtime_sequence >= 3);
    assert_eq!(provider.await.expect("provider task"), 1);
    drop(node);

    let replacement_key = SigningKey::from_bytes(&[51; 32]);
    let replacement = EdgeNode::new(
        enrollment.clone(),
        EdgeControlPlaneTrust::new(BTreeMap::from([(
            KEY_ID.into(),
            replacement_key.verifying_key(),
        )]))
        .expect("replacement trust"),
        EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("replacement store"),
        runtime(&runtime_state, &workspace, endpoint),
    )
    .expect("replacement node");
    let replayed = replacement
        .execute_task_token(&signed, now + 120_000)
        .await
        .expect("replayed receipt");
    assert_eq!(replayed, receipt);
    let outbox = replacement.pending_outbox(0, 10).expect("outbox");
    assert!(matches!(
        &outbox[0].payload,
        EdgeOutboxPayload::TaskReceipt(receipt)
            if receipt.status == EdgeTaskReceiptStatus::Accepted
    ));
    assert!(matches!(
        &outbox[outbox.len() - 1].payload,
        EdgeOutboxPayload::TaskReceipt(receipt)
            if receipt.status == EdgeTaskReceiptStatus::Succeeded
    ));
    let runtime_events = outbox
        .iter()
        .filter_map(|record| match &record.payload {
            EdgeOutboxPayload::RuntimeEvent(event) => Some(event),
            EdgeOutboxPayload::TaskReceipt(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runtime_events.len() as u64, receipt.last_runtime_sequence);
    assert!(runtime_events.iter().all(|event| {
        event.task_id == Uuid::from_u128(207)
            && event.node_id == Uuid::from_u128(208)
            && event.node_generation == 3
            && event.invocation == invocation()
            && event.session_id == run_id
            && !event.attempt_id.is_nil()
            && event.digest.len() == 64
    }));
}

/// The production break this catches is replaying model or Tool work after the
/// Runtime durably reached a terminal event but the node crashed before writing
/// its own terminal receipt. Recovery must derive the receipt from Runtime
/// evidence and must not issue a second provider request.
#[tokio::test]
async fn a_terminal_runtime_event_closes_the_receipt_crash_window_without_reexecution() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_provider().await;
    let key = SigningKey::from_bytes(&[52; 32]);
    let run_id = Uuid::from_u128(210);
    let now = Utc::now().timestamp_millis();
    let enrollment = enrollment(edge_state.path(), now);
    let signed = token(&key, &enrollment, now, run_id);
    let trust = EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
        .expect("trust");
    let verified = verify_edge_task_token_for_enrollment(&signed, &trust, &enrollment, now)
        .expect("verify signed task");
    let store = EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store");
    assert!(store.reserve(&verified).expect("reserve").is_new());

    let first_runtime = runtime(&runtime_state, &workspace, endpoint);
    let outcome = first_runtime
        .execute_at_epoch(invocation(), run_id, &verified.claims.input, 17)
        .await
        .expect("Runtime execution");
    assert_eq!(outcome.status, agent_protocol::RunStatus::Succeeded);
    assert_eq!(provider.await.expect("provider task"), 1);
    drop(first_runtime);
    drop(store);

    let replacement = EdgeNode::new(
        enrollment.clone(),
        EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
            .expect("replacement trust"),
        EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("replacement store"),
        runtime(
            &runtime_state,
            &workspace,
            "http://127.0.0.1:1/v1/chat/completions".into(),
        ),
    )
    .expect("replacement node");

    let receipt = replacement
        .execute_task_token(&signed, now + 120_000)
        .await
        .expect("reconciled receipt");
    assert_eq!(receipt.status, EdgeTaskReceiptStatus::Succeeded);
    assert_eq!(receipt.output, "edge-ok");
    assert!(receipt.last_runtime_sequence >= 3);
}

/// The production break this catches is reopening one durable node state root
/// under a different node identity or generation. A task ledger cannot be
/// transferred between enrollments merely by changing process arguments.
#[test]
fn a_durable_state_root_is_bound_to_one_node_identity_and_generation() {
    let edge_state = tempfile::tempdir().expect("edge state");
    let runtime_state = tempfile::tempdir().expect("runtime state");
    let workspace = tempfile::tempdir().expect("workspace");
    let key = SigningKey::from_bytes(&[53; 32]);
    let now = Utc::now().timestamp_millis();
    let enrollment = enrollment(edge_state.path(), now);
    let node = EdgeNode::new(
        enrollment.clone(),
        EdgeControlPlaneTrust::new(BTreeMap::from([(KEY_ID.into(), key.verifying_key())]))
            .expect("trust"),
        EdgeNodeStore::open_enrolled(edge_state.path(), &enrollment).expect("store"),
        runtime(
            &runtime_state,
            &workspace,
            "http://127.0.0.1:1/v1/chat/completions".into(),
        ),
    )
    .expect("first node");
    drop(node);

    let wrong = common::verified_enrollment(
        edge_state.path(),
        Uuid::from_u128(9990),
        Uuid::from_u128(999),
        4,
        now,
    );
    assert!(EdgeNodeStore::open_enrolled(edge_state.path(), &wrong).is_err());
}
