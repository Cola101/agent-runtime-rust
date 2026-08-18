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
    InitializedRuntimeClient, RUNTIME_CAPABILITY_EVENTS_WATCH, RUNTIME_CAPABILITY_RUN_CONTROL,
    RUNTIME_CAPABILITY_RUN_SUBMIT, RUNTIME_CAPABILITY_SESSION_CONTINUE,
    RUNTIME_CAPABILITY_SESSION_FORK, RUNTIME_CAPABILITY_SESSION_HISTORY,
    RUNTIME_CAPABILITY_SESSION_LIST, RUNTIME_CAPABILITY_SESSION_READ,
    RUNTIME_CAPABILITY_SESSION_ROLLBACK, RUNTIME_CAPABILITY_SESSION_START,
    RUNTIME_CLIENT_CONTRACT_VERSION, RUNTIME_CLIENT_SCHEMA_VERSION, RuntimeClient,
    RuntimeClientErrorCode, RuntimeClientEventCursorRequest, RuntimeClientHello,
    RuntimeSessionForkRequest, RuntimeSessionHistoryRequest, RuntimeSessionListRequest,
    RuntimeSessionReadRequest, RuntimeSessionRollbackRequest, RuntimeSessionTurnRequest,
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const MODEL_REPLY: &str = "headless Runtime client is ready";

async fn spawn_provider(response_delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(response_delay).await;
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

fn runtime_with_limits(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    invocations: &[RuntimeInvocationContext],
    provider_endpoint: &str,
    limits: RuntimeAdmissionLimits,
) -> EmbeddedRuntime {
    let config = LocalRuntimeConfig {
        state_root: state_root.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
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
                endpoint: provider_endpoint.into(),
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
    };
    EmbeddedRuntime::new(
        limits,
        invocations
            .iter()
            .copied()
            .map(|invocation| RuntimeProfile {
                invocation,
                config: config.clone(),
            })
            .collect(),
    )
    .expect("Runtime")
}

/// Room for everything the ordinary acceptance path starts at once.
fn roomy_limits() -> RuntimeAdmissionLimits {
    RuntimeAdmissionLimits {
        max_active_runs: 4,
        max_active_runs_per_tenant: 4,
        max_active_runs_per_workspace: 2,
        max_queued_runs: 8,
        max_queued_runs_per_tenant: 8,
    }
}

fn runtime(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    invocations: &[RuntimeInvocationContext],
    provider_endpoint: &str,
) -> EmbeddedRuntime {
    runtime_with_limits(
        state_root,
        workspace_root,
        invocations,
        provider_endpoint,
        roomy_limits(),
    )
}

/// Waits until this handle is the only one left, so the state-root lease is
/// about to be released rather than merely likely to be.
///
/// Not a settling delay: it converges the instant the last background task
/// ends, and the bound only turns a hang into a readable failure.
async fn wait_released(runtime: &Arc<EmbeddedRuntime>) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while Arc::strong_count(runtime) > 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the previous Runtime never released its state root");
}

/// Named apart from the `client` bindings that hold its result. `let client =
/// client(..)` shadows this for the rest of the function, and `drop` does not
/// end a binding -- a later call then resolves to the value, not the helper.
fn initialized_client(
    runtime: Arc<EmbeddedRuntime>,
    required_capabilities: &[&str],
) -> InitializedRuntimeClient {
    RuntimeClient::new(runtime)
        .initialize(&RuntimeClientHello {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            min_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            max_contract_version: RUNTIME_CLIENT_CONTRACT_VERSION,
            required_capabilities: required_capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        })
        .expect("compatible client contract")
}

async fn wait_terminal(
    client: &InitializedRuntimeClient,
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
) -> RunStatus {
    let mut stream = client
        .watch_events(invocation, run_id, 0, 16)
        .expect("watch");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match stream
                .recv()
                .await
                .expect("stream ended")
                .expect("stream item")
            {
                RuntimeEventStreamItem::Boundary {
                    state: RuntimeEventCursorState::Terminal { status },
                    ..
                }
                | RuntimeEventStreamItem::Boundary {
                    state: RuntimeEventCursorState::Retired { status, .. },
                    ..
                } => break status,
                _ => {}
            }
        }
    })
    .await
    .expect("terminal boundary")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_client_initializes_submits_and_streams_a_real_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let provider_endpoint = spawn_provider(Duration::ZERO).await;
    let runtime = runtime(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
    );

    // The integration consumer receives this port, not an EmbeddedRuntime or
    // any path/credential/configuration object.
    let client = initialized_client(
        Arc::new(runtime),
        &[
            RUNTIME_CAPABILITY_RUN_SUBMIT,
            RUNTIME_CAPABILITY_RUN_CONTROL,
            RUNTIME_CAPABILITY_EVENTS_WATCH,
        ],
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn a_session_client_is_retry_safe_fenced_paged_and_restartable() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation_a = invocation();
    let invocation_b = RuntimeInvocationContext {
        workload_identity_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        ..invocation_a
    };
    let provider_endpoint = spawn_provider(Duration::from_millis(100)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_EVENTS_WATCH,
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_CONTINUE,
        RUNTIME_CAPABILITY_SESSION_FORK,
        RUNTIME_CAPABILITY_SESSION_ROLLBACK,
        RUNTIME_CAPABILITY_SESSION_READ,
        RUNTIME_CAPABILITY_SESSION_LIST,
        RUNTIME_CAPABILITY_SESSION_HISTORY,
    ];
    let runtime_a = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[invocation_a, invocation_b],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&runtime_a), &capabilities);

    let session_id = Uuid::now_v7();
    let source_branch_id = Uuid::now_v7();
    let first_run_id = Uuid::now_v7();
    let start = RuntimeSessionTurnRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation: invocation_a,
        session_id,
        branch_id: source_branch_id,
        generation: 1,
        run_id: first_run_id,
        input: "first durable Session Turn".into(),
    };
    let accepted = client
        .start_session(start.clone())
        .await
        .expect("Session start accepted");
    assert_eq!(accepted.head.active_run_id, Some(first_run_id));
    assert_eq!(
        wait_terminal(&client, invocation_a, first_run_id).await,
        RunStatus::Succeeded
    );
    let first_head = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: source_branch_id,
        })
        .expect("first head");
    assert_eq!(first_head.turn_count, 1);
    assert_eq!(first_head.active_run_id, None);
    let replayed = client
        .start_session(start)
        .await
        .expect("identical Session start retry");
    assert_eq!(replayed.run_id, first_run_id);
    assert_eq!(replayed.head, first_head);

    let second_a = RuntimeSessionTurnRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation: invocation_a,
        session_id,
        branch_id: source_branch_id,
        generation: 1,
        run_id: Uuid::now_v7(),
        input: "only one concurrent continuation may win".into(),
    };
    let second_b = RuntimeSessionTurnRequest {
        run_id: Uuid::now_v7(),
        input: "the competing continuation must be fenced".into(),
        ..second_a.clone()
    };
    let (left, right) = tokio::join!(
        client.continue_session(second_a.clone()),
        client.continue_session(second_b.clone())
    );
    let (winner, loser) = match (left, right) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        other => panic!("exactly one continuation must win: {other:?}"),
    };
    assert_eq!(loser.code, RuntimeClientErrorCode::Conflict);
    assert_eq!(
        wait_terminal(&client, invocation_a, winner.run_id).await,
        RunStatus::Succeeded
    );
    let source = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: source_branch_id,
        })
        .expect("source head");
    assert_eq!(source.turn_count, 2);

    let fork_branch_id = Uuid::now_v7();
    let fork_request = RuntimeSessionForkRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation: invocation_a,
        session_id,
        source_branch_id,
        source_generation: source.generation,
        through_turn_ordinal: 1,
        target_branch_id: fork_branch_id,
    };
    let fork = client
        .fork_session(fork_request.clone())
        .await
        .expect("Fork");
    assert_eq!(fork.turn_count, 1);
    assert_eq!(
        client
            .fork_session(fork_request)
            .await
            .expect("idempotent Fork"),
        fork
    );
    let fork_run_id = Uuid::now_v7();
    client
        .continue_session(RuntimeSessionTurnRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: fork_branch_id,
            generation: fork.generation,
            run_id: fork_run_id,
            input: "continue only the fork".into(),
        })
        .await
        .expect("fork continuation");
    assert_eq!(
        wait_terminal(&client, invocation_a, fork_run_id).await,
        RunStatus::Succeeded
    );

    let rollback_request = RuntimeSessionRollbackRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation: invocation_a,
        session_id,
        branch_id: source_branch_id,
        generation: source.generation,
        through_turn_ordinal: 1,
    };
    let rolled = client
        .rollback_session(rollback_request.clone())
        .await
        .expect("Rollback");
    assert_eq!(rolled.generation, 2);
    assert_eq!(rolled.turn_count, 1);
    assert_eq!(
        client
            .rollback_session(rollback_request)
            .await
            .expect("idempotent Rollback"),
        rolled
    );

    let list = client
        .list_sessions(RuntimeSessionListRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            after_session_id: None,
            after_branch_id: None,
            limit: 1,
        })
        .expect("first Session page");
    assert_eq!(list.heads.len(), 1);
    assert!(list.next_after_session_id.is_some());
    let second_page = client
        .list_sessions(RuntimeSessionListRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            after_session_id: list.next_after_session_id,
            after_branch_id: list.next_after_branch_id,
            limit: 1,
        })
        .expect("second Session page");
    assert_eq!(second_page.heads.len(), 1);
    assert_eq!(
        client
            .list_sessions(RuntimeSessionListRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation: invocation_b,
                after_session_id: None,
                after_branch_id: None,
                limit: 10,
            })
            .expect("other invocation list")
            .heads,
        Vec::new()
    );
    assert_eq!(
        client
            .read_session(RuntimeSessionReadRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation: invocation_b,
                session_id,
                branch_id: source_branch_id,
            })
            .expect_err("another invocation must not read the Session")
            .code,
        RuntimeClientErrorCode::NotFound
    );

    let archived = client
        .read_session_history(RuntimeSessionHistoryRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: source_branch_id,
            generation: 1,
            after_turn_ordinal: 0,
            limit: 1,
        })
        .expect("archived history page");
    assert_eq!(archived.turns.len(), 1);
    assert_eq!(archived.next_after_turn_ordinal, Some(1));
    // Replacing a Runtime over the same state root means the previous one has
    // let go of it. Its `flock` lease lives in the `EmbeddedRuntime`, and a
    // Turn's background task holds an `Arc` to that Runtime past the Turn's
    // terminal event -- so dropping the client is not, on its own, the end of
    // the old Runtime. Waiting on the exact reference count is the observable
    // that says it is; a sleep here would only be hiding that it is not.
    drop(client);
    wait_released(&runtime_a).await;
    drop(runtime_a);

    let replacement = initialized_client(
        Arc::new(runtime(
            state.path(),
            workspace.path(),
            &[invocation_a, invocation_b],
            &provider_endpoint,
        )),
        &capabilities,
    );
    let restored = replacement
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: source_branch_id,
        })
        .expect("Session survives Runtime replacement");
    assert_eq!(restored, rolled);
    let post_restart_run_id = Uuid::now_v7();
    replacement
        .continue_session(RuntimeSessionTurnRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: invocation_a,
            session_id,
            branch_id: source_branch_id,
            generation: restored.generation,
            run_id: post_restart_run_id,
            input: "continue after Runtime replacement".into(),
        })
        .await
        .expect("post-restart continuation");
    assert_eq!(
        wait_terminal(&replacement, invocation_a, post_restart_run_id).await,
        RunStatus::Succeeded
    );
}

/// A refused Turn must leave the branch exactly as it found it.
///
/// The accept path persists the active Turn before it has a Run to point at:
/// `prepare_session_*` writes the Session record, and only afterwards does the
/// caller take Run ownership, acquire admission and persist the Run record. If
/// any of those refuses, the branch is left holding an active Turn whose Run
/// was never created -- and that branch is finished. `continue` is refused for
/// an active Turn conflict forever, and there is no Run to approve, cancel or
/// complete, because there is no Run.
///
/// Nothing here waits on a duration. The provider is given a delay far longer
/// than the test so an admitted Turn holds its slot for the whole of it, and
/// the queue's fullness is read from the admission snapshot rather than
/// assumed to have happened by now.
#[tokio::test]
async fn a_refused_admission_leaves_no_active_turn_and_no_orphan_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let provider_endpoint = spawn_provider(Duration::from_secs(3_600)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_CONTINUE,
        RUNTIME_CAPABILITY_SESSION_READ,
    ];
    let runtime = Arc::new(runtime_with_limits(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
    ));
    let client = Arc::new(initialized_client(Arc::clone(&runtime), &capabilities));

    let occupying = RuntimeSessionTurnRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation,
        session_id: Uuid::now_v7(),
        branch_id: Uuid::now_v7(),
        generation: 1,
        run_id: Uuid::now_v7(),
        input: "hold the only active slot".into(),
    };
    client
        .start_session(occupying.clone())
        .await
        .expect("the first Session Turn is admitted");

    // Fills the single queue slot. This one parks inside admission and never
    // returns during the test, so it is spawned rather than awaited.
    let queued = RuntimeSessionTurnRequest {
        session_id: Uuid::now_v7(),
        branch_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        input: "fill the only queue slot".into(),
        ..occupying.clone()
    };
    let queueing = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.start_session(queued).await }
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.admission_snapshot().queued_runs == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second Session Turn never reached the queue");

    let refused = RuntimeSessionTurnRequest {
        session_id: Uuid::now_v7(),
        branch_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        input: "refused before it can run".into(),
        ..occupying.clone()
    };
    let error = client
        .start_session(refused.clone())
        .await
        .expect_err("a full queue must refuse the third Session Turn");
    // Its own code, not folded into `Unavailable`: a caller that is over its
    // ceiling can retry, and a caller whose state store is down cannot.
    assert_eq!(error.code, RuntimeClientErrorCode::ResourceExhausted);

    // The refusal is only honest if it left nothing behind. A Session that now
    // exists holds an active Turn for a Run that was never created.
    assert_eq!(
        client
            .read_session(RuntimeSessionReadRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation,
                session_id: refused.session_id,
                branch_id: refused.branch_id,
            })
            .expect_err("a refused Session Turn must not have created a Session")
            .code,
        RuntimeClientErrorCode::NotFound
    );

    queueing.abort();
}

/// A Run id names one Turn, not a slot to reuse.
///
/// Retry safety is what makes a caller-generated id worth having: a lost
/// response can be asked again. That guarantee only holds if the id is bound to
/// what it was accepted with. An id reused with different input is a different
/// Turn wearing an accepted Turn's name, and answering it from the old result
/// would silently drop the new input on the floor.
#[tokio::test]
async fn a_run_id_reused_with_different_input_is_refused() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let provider_endpoint = spawn_provider(Duration::from_millis(50)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_EVENTS_WATCH,
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_CONTINUE,
        RUNTIME_CAPABILITY_SESSION_READ,
    ];
    let runtime = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&runtime), &capabilities);

    let session_id = Uuid::now_v7();
    let branch_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let start = RuntimeSessionTurnRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation,
        session_id,
        branch_id,
        generation: 1,
        run_id,
        input: "the input this Run id was accepted with".into(),
    };
    client.start_session(start.clone()).await.expect("start");
    assert_eq!(
        wait_terminal(&client, invocation, run_id).await,
        RunStatus::Succeeded
    );

    // Byte-identical: answered from what exists.
    let replayed = client
        .start_session(start.clone())
        .await
        .expect("an identical retry is the same Turn");
    assert_eq!(replayed.run_id, run_id);

    // Same id, different input: refused rather than answered from the old
    // result, and refused on both entry points.
    let mutated = RuntimeSessionTurnRequest {
        input: "a different input under the same Run id".into(),
        ..start.clone()
    };
    assert_eq!(
        client
            .start_session(mutated.clone())
            .await
            .expect_err("a reused Run id with different input must be refused")
            .code,
        RuntimeClientErrorCode::Conflict
    );
    assert_eq!(
        client
            .continue_session(RuntimeSessionTurnRequest {
                generation: 1,
                ..mutated
            })
            .await
            .expect_err("continue must refuse it for the same reason")
            .code,
        RuntimeClientErrorCode::Conflict
    );

    // And the refusal changed nothing: the Turn that was accepted is still the
    // one on the branch.
    let head = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
        })
        .expect("head");
    assert_eq!(head.turn_count, 1);
    assert_eq!(head.active_run_id, None);
}

/// Rollback is retry-safe exactly once, and stops being so the moment the
/// branch moves on.
///
/// A caller that loses the response to a rollback must be able to ask again.
/// But "ask again" and "roll back a second time" look identical on the wire,
/// so the retry is only honoured while the branch still looks the way the
/// first rollback left it: one generation later, history exactly the prefix,
/// nothing active. Once a Turn has been appended past it, replaying the old
/// request must conflict rather than quietly discard that Turn.
#[tokio::test]
async fn a_rollback_is_replayable_until_the_branch_moves_past_it() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let provider_endpoint = spawn_provider(Duration::from_millis(50)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_EVENTS_WATCH,
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_CONTINUE,
        RUNTIME_CAPABILITY_SESSION_ROLLBACK,
        RUNTIME_CAPABILITY_SESSION_READ,
    ];
    let runtime = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&runtime), &capabilities);

    let session_id = Uuid::now_v7();
    let branch_id = Uuid::now_v7();
    let turn = |generation: u64, text: &str| RuntimeSessionTurnRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation,
        session_id,
        branch_id,
        generation,
        run_id: Uuid::now_v7(),
        input: text.to_owned(),
    };

    let first = turn(1, "first turn");
    let first_run_id = first.run_id;
    client.start_session(first).await.expect("start");
    assert_eq!(
        wait_terminal(&client, invocation, first_run_id).await,
        RunStatus::Succeeded
    );
    let second = turn(1, "second turn");
    let second_run_id = second.run_id;
    client.continue_session(second).await.expect("continue");
    assert_eq!(
        wait_terminal(&client, invocation, second_run_id).await,
        RunStatus::Succeeded
    );

    let rollback = RuntimeSessionRollbackRequest {
        schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
        invocation,
        session_id,
        branch_id,
        generation: 1,
        through_turn_ordinal: 1,
    };
    let rolled = client
        .rollback_session(rollback.clone())
        .await
        .expect("rollback");
    assert_eq!(rolled.turn_count, 1);
    assert_eq!(rolled.generation, 2, "a rollback advances the generation");

    // The lost-response case: the same request again is the same rollback.
    let replayed = client
        .rollback_session(rollback.clone())
        .await
        .expect("an identical rollback retry is the same rollback");
    assert_eq!(replayed, rolled);

    // Now the branch moves past it.
    let third = turn(2, "a turn after the rollback");
    let third_run_id = third.run_id;
    client
        .continue_session(third)
        .await
        .expect("continue after rollback");
    assert_eq!(
        wait_terminal(&client, invocation, third_run_id).await,
        RunStatus::Succeeded
    );

    // Replaying the old rollback now would discard that Turn. It must not be
    // mistaken for a retry.
    assert_eq!(
        client
            .rollback_session(rollback)
            .await
            .expect_err("a rollback replayed past its own generation must conflict")
            .code,
        RuntimeClientErrorCode::Conflict
    );
    let head = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
        })
        .expect("head");
    assert_eq!(head.turn_count, 2, "the refused replay kept the later Turn");
}

/// A Provider that counts what it was actually asked, so "recovery did not
/// re-request the model" can be asserted rather than assumed.
async fn spawn_counting_provider(response_delay: Duration) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(response_delay).await;
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
    (endpoint, requests)
}

fn session_record_path(state_root: &std::path::Path, session_id: Uuid) -> std::path::PathBuf {
    state_root
        .join("sessions")
        .join(session_id.to_string())
        .join("session.json")
}

/// The crash window, reconstructed on disk rather than raced for.
///
/// A Turn's Checkpoint is durable before its Turn is committed onto the branch
/// head. A process that dies in between leaves a branch holding an active Turn
/// whose Run is already finished -- and the only honest way to finish it is
/// from the Checkpoint, because asking the model again would bill a second time
/// for an answer already given, and replaying its Tools would repeat effects
/// that already landed.
///
/// The window is produced by putting the active Turn back into the durable
/// record after the Turn completed, which is exactly the state a crash leaves
/// behind, and is deterministic in a way that racing a real crash is not.
#[tokio::test]
async fn a_terminal_turn_lost_before_its_head_is_finished_from_the_checkpoint() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let invocation = invocation();
    let (provider_endpoint, requests) = spawn_counting_provider(Duration::from_millis(20)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_EVENTS_WATCH,
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_CONTINUE,
        RUNTIME_CAPABILITY_SESSION_READ,
    ];

    let session_id = Uuid::now_v7();
    let branch_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let first = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&first), &capabilities);
    client
        .start_session(RuntimeSessionTurnRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
            generation: 1,
            run_id,
            input: "a Turn whose head never got committed".into(),
        })
        .await
        .expect("start");
    assert_eq!(
        wait_terminal(&client, invocation, run_id).await,
        RunStatus::Succeeded
    );
    let committed = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
        })
        .expect("committed head");
    assert_eq!(committed.turn_count, 1);
    drop(client);
    wait_released(&first).await;
    drop(first);

    let asked_before_recovery = requests.load(Ordering::SeqCst);
    assert!(asked_before_recovery >= 1, "the Turn really ran");

    // Rewind the durable record to the instant before the head was committed:
    // the Turn is gone from history and active again, while its Checkpoint and
    // terminal events stay exactly where the completed Run left them.
    let path = session_record_path(state.path(), session_id);
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("session record")).expect("json");
    let branch = &mut record["branches"][branch_id.to_string()];
    assert_eq!(branch["history"].as_array().expect("history").len(), 1);
    branch["active_turn"] = serde_json::json!({
        "run_id": run_id.to_string(),
        "generation": 1,
        // The digest of the history this Turn was accepted against, which for a
        // first Turn is the empty one. Taken from the protocol rather than
        // rebuilt here: a hand-rolled hash would test the test.
        "history_digest": agent_protocol::session_conversation_history_digest(&[]),
        "input": "a Turn whose head never got committed",
    });
    branch["history"] = serde_json::json!([]);
    std::fs::write(&path, serde_json::to_vec(&record).expect("encode")).expect("write");

    // A replacement Runtime over the same state root, as after a restart.
    let replacement = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[invocation],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&replacement), &capabilities);
    let report = client.recover_on_startup().await;
    assert!(
        report.failures.is_empty(),
        "startup recovery reported failures: {:?}",
        report.failures
    );

    let restored = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
        })
        .expect("restored head");
    assert_eq!(
        restored.turn_count, 1,
        "the lost Turn is finished from its Checkpoint, not dropped"
    );
    assert_eq!(restored.active_run_id, None);
    assert_eq!(
        restored.history_digest, committed.history_digest,
        "recovery reconstructs the same history, not a different one"
    );

    // The whole point: nothing was asked of the model to get there.
    assert_eq!(
        requests.load(Ordering::SeqCst),
        asked_before_recovery,
        "recovery re-requested the model instead of reading its Checkpoint"
    );

    // And the branch is continuable again rather than stuck on a finished Turn.
    let next_run_id = Uuid::now_v7();
    client
        .continue_session(RuntimeSessionTurnRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation,
            session_id,
            branch_id,
            generation: 1,
            run_id: next_run_id,
            input: "the branch still works".into(),
        })
        .await
        .expect("continue after recovery");
    assert_eq!(
        wait_terminal(&client, invocation, next_run_id).await,
        RunStatus::Succeeded
    );
}

/// A v1 Session record is adopted only by the local default identity.
///
/// v1 predates per-invocation binding, so a record at that version carries no
/// trustworthy owner. Reading it as though it belonged to whoever happened to
/// ask would hand one caller another's conversation on the strength of a
/// version number. The only identity a v1 record may be read as is the
/// built-in local one, which is the identity it was actually written by.
#[tokio::test]
async fn a_legacy_session_record_is_adopted_only_by_the_local_default_identity() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let local = agent_runtime_host::local_invocation_context();
    // Derived from the local identity rather than generated: one state root
    // belongs to one Workspace identity, so a wholly unrelated invocation
    // cannot be hosted beside it. Everything else differs, which is all the
    // rule under test needs -- this is simply not the local default.
    let other = RuntimeInvocationContext {
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
        ..local
    };
    let provider_endpoint = spawn_provider(Duration::from_millis(20)).await;
    let capabilities = [
        RUNTIME_CAPABILITY_EVENTS_WATCH,
        RUNTIME_CAPABILITY_SESSION_START,
        RUNTIME_CAPABILITY_SESSION_READ,
    ];
    let runtime = Arc::new(runtime(
        state.path(),
        workspace.path(),
        &[local, other],
        &provider_endpoint,
    ));
    let client = initialized_client(Arc::clone(&runtime), &capabilities);

    // Two real Sessions, one per identity, so the records under test are
    // genuine rather than hand-built.
    let sow = |invocation: RuntimeInvocationContext| {
        let session_id = Uuid::now_v7();
        let branch_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        (
            session_id,
            branch_id,
            run_id,
            RuntimeSessionTurnRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation,
                session_id,
                branch_id,
                generation: 1,
                run_id,
                input: "a Turn written before per-invocation binding".into(),
            },
        )
    };

    let (local_session, local_branch, local_run, local_start) = sow(local);
    client
        .start_session(local_start)
        .await
        .expect("local start");
    assert_eq!(
        wait_terminal(&client, local, local_run).await,
        RunStatus::Succeeded
    );
    let (other_session, other_branch, other_run, other_start) = sow(other);
    client
        .start_session(other_start)
        .await
        .expect("other start");
    assert_eq!(
        wait_terminal(&client, other, other_run).await,
        RunStatus::Succeeded
    );

    // Rewind both records to the v1 shape: the version drops, and with it the
    // field that says who owns the Session.
    let downgrade = |session_id: Uuid| {
        let path = session_record_path(state.path(), session_id);
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("json");
        record["store_version"] = serde_json::json!(1);
        record.as_object_mut().expect("object").remove("invocation");
        std::fs::write(&path, serde_json::to_vec(&record).expect("encode")).expect("write");
    };
    downgrade(local_session);
    downgrade(other_session);

    // The local default reads its own v1 record.
    let migrated = client
        .read_session(RuntimeSessionReadRequest {
            schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
            invocation: local,
            session_id: local_session,
            branch_id: local_branch,
        })
        .expect("a v1 record is readable as the local default identity");
    assert_eq!(migrated.turn_count, 1);

    // A tenant does not, even though the record is now version 1 and silent
    // about its owner. Absence of an owner is not consent to be adopted.
    assert_eq!(
        client
            .read_session(RuntimeSessionReadRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation: other,
                session_id: other_session,
                branch_id: other_branch,
            })
            .expect_err("a v1 record must not be adopted by any other identity")
            .code,
        RuntimeClientErrorCode::NotFound
    );

    // Nor may one identity reach the other's Session by claiming its id.
    assert_eq!(
        client
            .read_session(RuntimeSessionReadRequest {
                schema_version: RUNTIME_CLIENT_SCHEMA_VERSION,
                invocation: other,
                session_id: local_session,
                branch_id: local_branch,
            })
            .expect_err("another identity must not read the local default's Session")
            .code,
        RuntimeClientErrorCode::NotFound
    );
}
