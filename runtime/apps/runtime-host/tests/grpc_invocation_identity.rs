//! The network invocation surface must prove who is calling before it starts,
//! steers or reveals a Run.
//!
//! This is the first surface on which the Runtime is reachable from another
//! machine, so the questions it has to answer are different from the local
//! Unix-socket adapter's. There, being able to open the socket was the
//! authorization. Here it is a bearer token, and these tests pin the exact
//! boundary: a Run-shaped token cannot act as an operator, a token for one
//! tenant cannot assert another, and a caller cannot reach a Profile that was
//! not registered for it.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use agent_runtime_invocation_protocol::v1::{
    ControlRunRequest, InitializeRuntimeRequest, ReadRunEventsRequest, RuntimeInvocationRef,
    SubmitRunRequest,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("agent-runtime-grpc-{}", Uuid::now_v7()));
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        Self(root)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// An operator identity: it names who is acting and for which tenant, and every
/// Run-scoped field is absent (ADR-0121).
fn operator_claims(tenant_id: Uuid, scopes: &[&str]) -> WorkloadIdentityClaims {
    let now = chrono::Utc::now().timestamp_millis();
    WorkloadIdentityClaims {
        schema_version: agent_workload_identity::OPERATOR_SCHEMA_VERSION,
        tenant_id,
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        run_id: Uuid::nil(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::nil(),
        worker_id: Uuid::nil(),
        worker_incarnation_id: Uuid::nil(),
        model_policy_id: Uuid::nil(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["runtime-host".to_owned()]),
        scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    }
}

/// A Run-shaped token (execution schema 4) carrying the invoke scope.
///
/// It must be valid in every respect *except* being an operator, otherwise the
/// test would pass for the wrong reason: an incomplete schema-4 token is
/// refused as malformed long before the shape is examined, which proves
/// nothing about the shape boundary. `model_policy_digest` in particular has
/// to be a real sha256 -- leaving it empty is what makes a fabricated schema-4
/// token invalid rather than merely Run-shaped.
fn run_shaped_claims(tenant_id: Uuid, scopes: &[&str]) -> WorkloadIdentityClaims {
    let mut claims = operator_claims(tenant_id, scopes);
    claims.schema_version = 4;
    claims.run_id = Uuid::now_v7();
    claims.attempt_id = Uuid::now_v7();
    claims.worker_id = Uuid::now_v7();
    claims.worker_incarnation_id = Uuid::now_v7();
    claims.session_id = Uuid::now_v7();
    claims.workspace_id = Uuid::now_v7();
    claims.agent_version_id = Uuid::now_v7();
    claims.model_policy_id = Uuid::now_v7();
    claims.model_policy_digest = "c".repeat(64);
    claims
}

fn sign(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key.sign(signing_input.as_bytes()).to_bytes());
    format!("{signing_input}.{signature}")
}

fn config(root: &TestRoot) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root: root.0.join("state"),
        workspace_root: root.0.join("workspace"),
        agent_instructions: "Answer from the current invocation only.".into(),
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
                // Never dialled by these tests: every one of them is refused at
                // the identity boundary, before any Run reaches a Provider.
                endpoint: "http://127.0.0.1:9/v1/chat/completions".into(),
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
    }
}

/// Registers exactly one Profile, for the identity the operator token carries.
async fn spawn_surface(
    signing_key: &SigningKey,
    root: &TestRoot,
    registered: RuntimeInvocationContext,
) -> String {
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 8,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation: registered,
            config: config(root),
        }],
    )
    .unwrap();
    let service = RuntimeInvocationGrpcService::new(
        Arc::new(runtime),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(RuntimeInvocationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    format!("http://{address}")
}

fn invocation_of(
    claims: &WorkloadIdentityClaims,
    profile: &RuntimeInvocationContext,
) -> RuntimeInvocationRef {
    RuntimeInvocationRef {
        schema_version: 1,
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
        workspace_id: profile.workspace_id.to_string(),
        agent_version_id: profile.agent_version_id.to_string(),
        model_policy_id: profile.model_policy_id.to_string(),
    }
}

fn profile_for(claims: &WorkloadIdentityClaims) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: claims.tenant_id,
        application_id: claims.application_id,
        workload_identity_id: claims.workload_identity_id,
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

fn with_token<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    request
}

fn submit(invocation: RuntimeInvocationRef) -> SubmitRunRequest {
    SubmitRunRequest {
        invocation: Some(invocation),
        run_id: Uuid::now_v7().to_string(),
        input: "hello".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initialization_negotiates_before_any_tenant_request() {
    let signing_key = SigningKey::from_bytes(&[70; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();

    let compatible = client
        .initialize(InitializeRuntimeRequest {
            schema_version: 1,
            min_contract_version: 1,
            max_contract_version: 1,
            required_capabilities: vec!["run.submit.v1".into()],
        })
        .await
        .expect("compatible initialize")
        .into_inner();
    assert_eq!(compatible.contract_version, 1);
    assert_eq!(
        compatible.capabilities,
        vec![
            "events.cursor.v1",
            "events.watch.v1",
            "recovery.startup.v1",
            "run.control.v1",
            "run.submit.v1",
        ]
    );

    let incompatible = client
        .initialize(InitializeRuntimeRequest {
            schema_version: 1,
            min_contract_version: 2,
            max_contract_version: 2,
            required_capabilities: Vec::new(),
        })
        .await
        .expect_err("version mismatch must fail before a Run starts");
    assert_eq!(incompatible.code(), Code::FailedPrecondition);

    let missing = client
        .initialize(InitializeRuntimeRequest {
            schema_version: 1,
            min_contract_version: 1,
            max_contract_version: 1,
            required_capabilities: vec!["desktop.magic.v1".into()],
        })
        .await
        .expect_err("a required capability cannot be guessed");
    assert_eq!(missing.code(), Code::FailedPrecondition);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_submit_without_a_workload_token_is_refused() {
    let signing_key = SigningKey::from_bytes(&[71; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();

    let status = client
        .submit(submit(invocation_of(&claims, &profile)))
        .await
        .expect_err("an unauthenticated submit must not start a Run");

    assert_eq!(status.code(), Code::Unauthenticated);
}

/// A Run token carrying `runtime.invoke` is still a Run. Without this the
/// separation between executing work and commissioning it would rest on a
/// scope grant alone (ADR-0121).
#[tokio::test(flavor = "multi_thread")]
async fn a_run_shaped_token_cannot_invoke_the_runtime() {
    let signing_key = SigningKey::from_bytes(&[72; 32]);
    let root = TestRoot::new();
    let operator = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&operator);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();

    let mut run_shaped = run_shaped_claims(operator.tenant_id, &[INVOKE_SCOPE]);
    run_shaped.application_id = operator.application_id;
    run_shaped.workload_identity_id = operator.workload_identity_id;
    let token = sign(&signing_key, &run_shaped);

    let status = client
        .submit(with_token(
            submit(invocation_of(&operator, &profile)),
            &token,
        ))
        .await
        .expect_err("a Run-shaped token must not reach the invocation surface");

    // Not Unauthenticated: the token is valid and re-authenticating cannot
    // turn a Run into an operator.
    assert_eq!(status.code(), Code::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_token_without_the_invoke_scope_is_refused() {
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &["mcp.oauth.admin"]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();
    let token = sign(&signing_key, &claims);

    let status = client
        .submit(with_token(submit(invocation_of(&claims, &profile)), &token))
        .await
        .expect_err("administering credentials does not imply commissioning Runs");

    assert_eq!(status.code(), Code::Unauthenticated);
}

/// The body may agree with the token, never widen it.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_cannot_assert_another_tenant_than_the_token() {
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();
    let token = sign(&signing_key, &claims);

    let mut asserted = invocation_of(&claims, &profile);
    asserted.tenant_id = Uuid::now_v7().to_string();

    let status = client
        .submit(with_token(submit(asserted), &token))
        .await
        .expect_err("a request body must not be able to name another tenant");

    assert_eq!(status.code(), Code::PermissionDenied);
}

/// Reaching an unregistered Profile is refused as `permission_denied`, not
/// `not_found`: whether a Profile exists is not something to probe for.
#[tokio::test(flavor = "multi_thread")]
async fn an_unregistered_profile_is_refused_without_confirming_it_exists() {
    let signing_key = SigningKey::from_bytes(&[75; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();
    let token = sign(&signing_key, &claims);

    let mut asserted = invocation_of(&claims, &profile);
    asserted.workspace_id = Uuid::now_v7().to_string();

    let status = client
        .submit(with_token(submit(asserted), &token))
        .await
        .expect_err("an unregistered Profile must not be invocable");

    assert_eq!(status.code(), Code::PermissionDenied);
    assert_eq!(status.message(), "this invocation is not registered");
}

/// Every RPC authenticates. A surface where only the mutating call checks would
/// let a token read another tenant's transcript.
#[tokio::test(flavor = "multi_thread")]
async fn reading_and_controlling_authenticate_the_same_way() {
    let signing_key = SigningKey::from_bytes(&[76; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();

    let read = client
        .read_events(ReadRunEventsRequest {
            schema_version: 1,
            invocation: Some(invocation_of(&claims, &profile)),
            run_id: Uuid::now_v7().to_string(),
            after_sequence: 0,
            limit: 16,
        })
        .await
        .expect_err("an unauthenticated read must not reveal a Run");
    assert_eq!(read.code(), Code::Unauthenticated);

    let control = client
        .control(ControlRunRequest {
            schema_version: 1,
            invocation: Some(invocation_of(&claims, &profile)),
            command_id: Uuid::now_v7().to_string(),
            run_id: Uuid::now_v7().to_string(),
            expected_owner_epoch: 1,
            action_json: br#"{"type":"cancel","reason":"stop"}"#.to_vec(),
        })
        .await
        .expect_err("an unauthenticated control must not steer a Run");
    assert_eq!(control.code(), Code::Unauthenticated);
}

/// A control action the Runtime does not define is refused at the edge rather
/// than reaching the durable command path.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_control_action_is_refused() {
    let signing_key = SigningKey::from_bytes(&[77; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();
    let token = sign(&signing_key, &claims);

    let status = client
        .control(with_token(
            ControlRunRequest {
                schema_version: 1,
                invocation: Some(invocation_of(&claims, &profile)),
                command_id: Uuid::now_v7().to_string(),
                run_id: Uuid::now_v7().to_string(),
                expected_owner_epoch: 1,
                action_json: br#"{"type":"delete_everything"}"#.to_vec(),
            },
            &token,
        ))
        .await
        .expect_err("an undefined action must not be accepted");

    assert_eq!(status.code(), Code::InvalidArgument);
}

/// The internal failure message must not describe this machine. `LocalRuntimeError`
/// carries state-root paths, and a network caller has no business learning them.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_never_leaks_a_host_path() {
    let signing_key = SigningKey::from_bytes(&[78; 32]);
    let root = TestRoot::new();
    let claims = operator_claims(Uuid::now_v7(), &[INVOKE_SCOPE]);
    let profile = profile_for(&claims);
    let endpoint = spawn_surface(&signing_key, &root, profile).await;
    let mut client = RuntimeInvocationClient::connect(endpoint).await.unwrap();
    let token = sign(&signing_key, &claims);

    let mut asserted = invocation_of(&claims, &profile);
    asserted.workspace_id = Uuid::now_v7().to_string();
    let status = client
        .submit(with_token(submit(asserted), &token))
        .await
        .unwrap_err();

    let root_fragment = root.0.to_string_lossy().to_string();
    assert!(
        !status.message().contains(&root_fragment),
        "a status message disclosed a host path: {}",
        status.message()
    );
    assert!(
        !status.message().contains("/var") && !status.message().contains("/tmp"),
        "a status message disclosed a filesystem location: {}",
        status.message()
    );
}
