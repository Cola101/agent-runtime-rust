//! The Session contract, driven entirely over the network.
//!
//! `runtime_client_contract` proves the same semantics in-process. This is the
//! other half of the claim that a Tauri, Electron, CLI or Java caller can use
//! one contract: the whole chain -- Initialize, Start, Watch, Continue, Fork,
//! Read, List, History -- performed by a caller holding nothing but a TCP
//! address and a bearer token, with no in-process handle to the Runtime at any
//! point.
//!
//! The Provider is a real loopback HTTP/SSE server, so the Turns genuinely
//! execute rather than being simulated.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::grpc::RuntimeInvocationGrpcService;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalToolConsent,
};
use agent_runtime_invocation_protocol::v1::run_event_stream_item::Item;
use agent_runtime_invocation_protocol::v1::run_lifecycle_boundary::Boundary;
use agent_runtime_invocation_protocol::v1::runtime_invocation_client::RuntimeInvocationClient;
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use agent_runtime_invocation_protocol::v1::{
    ForkSessionRequest, ListSessionsRequest, ReadSessionHistoryRequest, ReadSessionRequest,
    RollbackSessionRequest, SessionTurnRequest, WatchRunEventsRequest,
};
use agent_runtime_invocation_protocol::v1::{InitializeRuntimeRequest, RuntimeInvocationRef};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use uuid::Uuid;

const INVOKE_SCOPE: &str = "runtime.invoke";
const MODEL_REPLY: &str = "the runtime answered over grpc";

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

fn operator_claims(tenant_id: Uuid) -> WorkloadIdentityClaims {
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
        scopes: BTreeSet::from([INVOKE_SCOPE.to_owned()]),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    }
}

fn sign(signing_key: &SigningKey, claims: &WorkloadIdentityClaims) -> String {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key.sign(signing_input.as_bytes()).to_bytes());
    format!("{signing_input}.{signature}")
}

fn with_token<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")).unwrap(),
    );
    request
}

/// What the host logged while this test ran.
///
/// The reply a network caller gets is deliberately sanitised -- a host path
/// must not cross the client contract -- so a failure here said only "Session
/// storage is unavailable". The host does log the real error
/// (`note_state_root` in `client.rs`), but a test binary installs no
/// subscriber, so that line went nowhere and the cause of a flake this test's
/// own comment says has been seen "for weeks, under load" stayed unknown.
#[derive(Clone, Default)]
struct HostLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl HostLog {
    /// Process-wide rather than per-thread: the host work happens on Tokio's
    /// threads and `set_default` would only cover this one.
    fn install() -> Self {
        let held = Self::default();
        let _ = tracing_subscriber::fmt()
            .with_writer(held.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .try_init();
        held
    }

    fn said(&self) -> String {
        let held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let text = String::from_utf8_lossy(&held).into_owned();
        if text.trim().is_empty() {
            "(nothing at WARN or above)".into()
        } else {
            text
        }
    }
}

impl std::io::Write for HostLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for HostLog {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

/// One caller, one token, the whole Session surface.
#[tokio::test(flavor = "multi_thread")]
async fn a_network_caller_starts_continues_forks_and_reads_a_real_session() {
    let host_log = HostLog::install();
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let provider_endpoint = spawn_provider().await;

    let claims = operator_claims(Uuid::now_v7());
    let token = sign(&signing_key, &claims);
    let profile = RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: claims.tenant_id,
        application_id: claims.application_id,
        workload_identity_id: claims.workload_identity_id,
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    };

    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 8,
            max_queued_runs_per_tenant: 4,
        },
        vec![RuntimeProfile {
            invocation: profile,
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
    .expect("runtime");

    let service = RuntimeInvocationGrpcService::new(
        Arc::new(runtime),
        WorkloadTokenVerifier::new(signing_key.verifying_key()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RuntimeInvocationServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });

    // From here on the caller holds only an address and a token.
    let mut client = RuntimeInvocationClient::connect(format!("http://{address}"))
        .await
        .expect("connect");

    // Every Session capability is required up front. A caller that discovers
    // halfway through a chain that fork is unavailable has already started a
    // Session it cannot finish.
    let initialized = client
        .initialize(InitializeRuntimeRequest {
            schema_version: 1,
            min_contract_version: 1,
            max_contract_version: 1,
            required_capabilities: vec![
                "session.start.v1".into(),
                "session.continue.v1".into(),
                "session.fork.v1".into(),
                "session.rollback.v1".into(),
                "session.read.v1".into(),
                "session.list.v1".into(),
                "session.history.v1".into(),
            ],
        })
        .await
        .expect("initialize")
        .into_inner();
    assert_eq!(initialized.contract_version, 1);
    // The same ceilings the in-process client publishes, so a caller does not
    // have to learn them by being refused.
    assert_eq!(initialized.max_session_list_size, 256);
    assert_eq!(initialized.max_session_history_turns, 128);

    let invocation = RuntimeInvocationRef {
        schema_version: 1,
        tenant_id: claims.tenant_id.to_string(),
        application_id: claims.application_id.to_string(),
        workload_identity_id: claims.workload_identity_id.to_string(),
        workspace_id: profile.workspace_id.to_string(),
        agent_version_id: profile.agent_version_id.to_string(),
        model_policy_id: profile.model_policy_id.to_string(),
    };
    let session_id = Uuid::now_v7();
    let branch_id = Uuid::now_v7();

    // --- Start -----------------------------------------------------------
    let first_run_id = Uuid::now_v7();
    let started = client
        .start_session(with_token(
            SessionTurnRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
                generation: 1,
                run_id: first_run_id.to_string(),
                input: "first turn over the network".into(),
            },
            &token,
        ))
        .await
        .expect("start session")
        .into_inner();
    assert_eq!(started.run_id, first_run_id.to_string());

    // --- Watch -----------------------------------------------------------
    // Followed from the durable log, not from a broadcaster, and ended on the
    // typed boundary rather than on whichever event happened to arrive last.
    let status = watch_to_terminal(&mut client, &invocation, first_run_id, &token).await;
    assert_eq!(status, "succeeded");

    // --- Continue --------------------------------------------------------
    let second_run_id = Uuid::now_v7();
    let continued = client
        .continue_session(with_token(
            SessionTurnRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
                generation: 1,
                run_id: second_run_id.to_string(),
                input: "second turn over the network".into(),
            },
            &token,
        ))
        .await
        // The reply is deliberately sanitised -- a host path must not cross the
        // client contract -- so this failure has said only "Session storage is
        // unavailable" for weeks, under load, with no way to tell an exhausted
        // descriptor table from a transient ENOENT. The host logs the real
        // error now, but a test binary installs no tracing subscriber, so that
        // line goes nowhere here. What the test *can* say is what its own state
        // root looked like at the moment it was refused.
        .unwrap_or_else(|status| {
            panic!(
                "continue session: {status:?}\nstate root {} held {:?}\nhost said: {}",
                state.path().display(),
                std::fs::read_dir(state.path())
                    .map(|entries| entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.file_name())
                        .collect::<Vec<_>>())
                    .unwrap_or_default(),
                host_log.said(),
            )
        })
        .into_inner();
    assert_eq!(continued.run_id, second_run_id.to_string());
    assert_eq!(
        watch_to_terminal(&mut client, &invocation, second_run_id, &token).await,
        "succeeded"
    );

    // --- Read ------------------------------------------------------------
    let head = client
        .read_session(with_token(
            ReadSessionRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
            },
            &token,
        ))
        .await
        .expect("read session")
        .into_inner();
    assert_eq!(head.turn_count, 2);
    assert_eq!(head.active_run_id, None);

    // --- Fork ------------------------------------------------------------
    let fork_branch_id = Uuid::now_v7();
    let forked = client
        .fork_session(with_token(
            ForkSessionRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                source_branch_id: branch_id.to_string(),
                source_generation: head.generation,
                through_turn_ordinal: 1,
                target_branch_id: fork_branch_id.to_string(),
            },
            &token,
        ))
        .await
        .expect("fork session")
        .into_inner();
    assert_eq!(
        forked.turn_count, 1,
        "a fork carries only the prefix it named"
    );
    assert_eq!(forked.branch_id, fork_branch_id.to_string());

    // A fork replayed with the same target and prefix is the same fork, not a
    // second one: a caller that lost the response must be able to ask again.
    let replayed = client
        .fork_session(with_token(
            ForkSessionRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                source_branch_id: branch_id.to_string(),
                source_generation: head.generation,
                through_turn_ordinal: 1,
                target_branch_id: fork_branch_id.to_string(),
            },
            &token,
        ))
        .await
        .expect("fork retry")
        .into_inner();
    assert_eq!(replayed, forked);

    // --- List ------------------------------------------------------------
    let listed = client
        .list_sessions(with_token(
            ListSessionsRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                after_session_id: None,
                after_branch_id: None,
                limit: 16,
            },
            &token,
        ))
        .await
        .expect("list sessions")
        .into_inner();
    assert_eq!(listed.heads.len(), 2, "the branch and its fork");

    // Over the published ceiling is a caller error, refused as one rather than
    // quietly clamped -- a caller that thinks it asked for 512 and silently got
    // 256 will page past the end and stop early.
    let refused = client
        .list_sessions(with_token(
            ListSessionsRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                after_session_id: None,
                after_branch_id: None,
                limit: initialized.max_session_list_size + 1,
            },
            &token,
        ))
        .await
        .expect_err("a list limit above the published ceiling must be refused");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);

    // A cursor that names a Session without its branch is incomplete, not a
    // shorthand for "any branch".
    let half_cursor = client
        .list_sessions(with_token(
            ListSessionsRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                after_session_id: Some(session_id.to_string()),
                after_branch_id: None,
                limit: 16,
            },
            &token,
        ))
        .await
        .expect_err("half a paging cursor must be refused");
    assert_eq!(half_cursor.code(), tonic::Code::InvalidArgument);

    // --- History ---------------------------------------------------------
    let history = client
        .read_session_history(with_token(
            ReadSessionHistoryRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
                generation: head.generation,
                after_turn_ordinal: 0,
                limit: 1,
            },
            &token,
        ))
        .await
        .expect("read session history")
        .into_inner();
    assert_eq!(history.turns.len(), 1);
    assert_eq!(history.next_after_turn_ordinal, Some(1));

    // --- Rollback --------------------------------------------------------
    let rolled = client
        .rollback_session(with_token(
            RollbackSessionRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                session_id: session_id.to_string(),
                branch_id: branch_id.to_string(),
                generation: head.generation,
                through_turn_ordinal: 1,
            },
            &token,
        ))
        .await
        .expect("rollback session")
        .into_inner();
    assert_eq!(rolled.turn_count, 1);
    assert_eq!(
        rolled.generation,
        head.generation + 1,
        "a rollback advances the generation rather than rewriting the old one"
    );
}

/// Follows one Run from the durable log to its typed terminal boundary.
async fn watch_to_terminal(
    client: &mut RuntimeInvocationClient<tonic::transport::Channel>,
    invocation: &RuntimeInvocationRef,
    run_id: Uuid,
    token: &str,
) -> String {
    let mut stream = client
        .watch_events(with_token(
            WatchRunEventsRequest {
                schema_version: 1,
                invocation: Some(invocation.clone()),
                run_id: run_id.to_string(),
                after_sequence: 0,
                capacity: 16,
            },
            token,
        ))
        .await
        .expect("watch events")
        .into_inner();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(item) = stream.message().await.expect("stream item") {
            match item.item {
                Some(Item::Boundary(boundary)) => {
                    match boundary.lifecycle.and_then(|lifecycle| lifecycle.boundary) {
                        Some(Boundary::Terminal(terminal)) => return terminal.status,
                        Some(Boundary::Retired(retired)) => return retired.status,
                        _ => {}
                    }
                }
                Some(Item::Event(_)) | None => {}
            }
        }
        panic!("the stream ended before a terminal boundary");
    })
    .await
    .expect("the Turn did not reach a terminal boundary")
}
