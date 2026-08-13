//! Daemon-restart recovery (ADR-0035 decision 7, durability half).
//!
//! A client dying must not kill a Run — that is already covered. This file
//! covers the harder half: the daemon itself dying. The Checkpoint is on disk,
//! so a restarted daemon must pick the Run back up instead of leaving it
//! stranded, and must never re-execute a Run that already finished.

use agent_protocol::RunBudget;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRunState, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE, local_invocation_context,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use uuid::Uuid;

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Provider that strands its first caller and answers every later one. The
/// stranded connection stands in for a daemon that died mid-turn.
/// Returns the endpoint and a counter of requests it has accepted.
///
/// The counter matters: this provider strands only its *first* caller, and the
/// test's premise is that the caller stranded is the daemon that later crashes.
/// Without a way to observe that, the test can crash the first daemon before it
/// ever reached the provider, and then the *recovered* daemon becomes the one
/// that gets stranded -- which looks exactly like recovery being broken.
async fn spawn_stranding_provider() -> (String, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepted = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        let mut served = 0u32;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            served += 1;
            counter.store(served, Ordering::SeqCst);
            if served == 1 {
                // Hold the connection open forever without answering.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    drop(socket);
                });
                continue;
            }
            let body = text_turn("recovered answer");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        accepted,
    )
}

/// Answers the first caller and strands any accidental replay. This makes a
/// terminal event durable while letting the test detect whether recovery
/// incorrectly invokes the provider again.
async fn spawn_one_answer_provider() -> (String, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepted = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            let served = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if served > 1 {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                continue;
            }
            let body = text_turn("completed before cancellation");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        accepted,
    )
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    let mut config = LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Explain evidence before conclusions.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            endpoint,
            "local-test-model",
            "local-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    };
    // Recovery may consume one ambiguous in-flight attempt left by the
    // predecessor, but remains bounded across replacement daemons.
    config
        .model_routing
        .health_policy
        .max_same_provider_attempts = 2;
    config
}

async fn submit(socket: &Path, input: &str) -> Uuid {
    let response = request(
        socket,
        &LocalRequest::Submit {
            input: input.into(),
        },
    )
    .await;
    match response {
        LocalResponse::Accepted { run_id } => run_id,
        other => panic!("expected acceptance, got {other:?}"),
    }
}

async fn request(socket: &Path, request: &LocalRequest) -> LocalResponse {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request).expect("encode");
    line.push(b'\n');
    writer.write_all(&line).await.expect("write");
    writer.flush().await.expect("flush");
    let mut lines = BufReader::new(reader).lines();
    serde_json::from_str(&lines.next_line().await.expect("read").expect("line")).expect("decode")
}

/// Waits for an eventual condition.
///
/// The budget is sized for "how long before this counts as a hang", not for how
/// long the work is expected to take. It used to be 5s, which is the latter, and
/// under a full `cargo test --workspace` -- forty-odd test binaries competing for
/// cores -- a Run that spawns a tool binary and makes a provider round trip does
/// not reliably finish inside it. That produced a red suite three times with
/// nothing wrong, which is worse than a slow one: it makes "the suite is green"
/// useless as a signal. A longer budget costs nothing on the happy path, because
/// this returns as soon as the predicate holds.
async fn wait_for<F: Fn() -> bool>(label: &str, predicate: F) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {label}");
}

/// The production break this catches is a daemon recovering any record found
/// under a shared state root. A daemon bound to one Application/Workspace must
/// leave another identity's unfinished Run untouched.
#[tokio::test]
async fn a_daemon_does_not_recover_a_foreign_invocation_record() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let endpoint = spawn_one_answer_provider().await.0;
    let config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
    );
    let owner = local_invocation_context();
    let mut foreign = owner;
    foreign.application_id = Uuid::now_v7();
    foreign.workspace_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &agent_runtime_host::LocalRunRecord {
            store_version: 1,
            tenant_id: owner.tenant_id,
            application_id: owner.application_id,
            workload_identity_id: owner.workload_identity_id,
            workspace_id: owner.workspace_id,
            agent_version_id: owner.agent_version_id,
            model_policy_id: owner.model_policy_id,
            run_id,
            input: "owner-only run".into(),
            state: LocalRunState::Running,
            owner_epoch: 1,
        },
    )
    .unwrap();
    let daemon = LocalRuntimeDaemon::new_for_invocation(config, foreign).unwrap();

    assert_eq!(daemon.recover_unfinished().await.unwrap(), 0);
    assert_eq!(
        LocalRuntimeHost::read_run_record(state.path(), run_id)
            .unwrap()
            .unwrap()
            .state,
        LocalRunState::Running
    );
}

/// Runs a daemon on its own runtime, submits one Run, waits for it to become
/// durable, then drops the runtime. Dropping aborts every task the daemon
/// spawned, which is as close to a crash as a test can get in-process.
fn crash_after_first_checkpoint(config: LocalRuntimeConfig, accepted: Arc<AtomicU32>) -> Uuid {
    let state_root = config.state_root.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let socket = default_socket_path(&config.state_root);
            let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
            let daemon = LocalRuntimeDaemon::new(config);
            tokio::spawn(daemon.serve(listener));
            let run_id = submit(&socket, "Summarize the workspace.").await;
            // Both conditions: a Checkpoint on disk, and this daemon having
            // been the one the provider stranded.
            wait_for("the run to become durable and reach the provider", || {
                LocalRuntimeHost::checkpoint_path(&state_root, run_id).is_file()
                    && accepted.load(Ordering::SeqCst) >= 1
            })
            .await;
            run_id
        })
        // The runtime is dropped here: the daemon and its in-flight Run die.
    })
    .join()
    .expect("daemon thread")
}

/// Gets an operator cancellation acknowledged and then drops the whole runtime
/// before the in-flight Run gets another scheduling turn. The durable state
/// observed immediately after the acknowledgement is the crash-recovery
/// contract: an accepted cancellation must never still look resumable.
fn crash_after_cancel_ack(config: LocalRuntimeConfig, accepted: Arc<AtomicU32>) -> Uuid {
    let state_root = config.state_root.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let socket = default_socket_path(&config.state_root);
            let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
            let daemon = LocalRuntimeDaemon::new(config);
            tokio::spawn(daemon.serve(listener));
            let run_id = submit(&socket, "Summarize until cancelled.").await;
            wait_for("the run to checkpoint and reach the provider", || {
                LocalRuntimeHost::checkpoint_path(&state_root, run_id).is_file()
                    && accepted.load(Ordering::SeqCst) >= 1
            })
            .await;

            assert_eq!(
                request(&socket, &LocalRequest::Cancel { run_id }).await,
                LocalResponse::Accepted { run_id }
            );
            let acknowledged = LocalRuntimeHost::read_run_record(&state_root, run_id)
                .expect("record readable")
                .expect("record present after cancellation acknowledgement");
            assert_ne!(
                acknowledged.state,
                LocalRunState::Running,
                "an acknowledged cancellation must be durable before the daemon can crash"
            );
            run_id
        })
    })
    .join()
    .expect("daemon thread")
}

// Multi-threaded on purpose. `crash_after_first_checkpoint` blocks on `join`,
// and on the default current-thread runtime that also blocks the provider's
// accept loop, so the crashing daemon can never be the caller the provider
// strands. The test used to pass anyway, by an ordering accident: the daemon's
// connection sat in the kernel backlog until `join` returned, and was then
// accepted as a dead socket. Under load that accident does not hold and the
// *recovered* daemon became the stranded one, which looks exactly like recovery
// being broken.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_resumes_a_run_its_predecessor_left_unfinished() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_stranding_provider().await;

    let run_id = crash_after_first_checkpoint(
        config(state_root.clone(), workspace_root.clone(), endpoint.clone()),
        Arc::clone(&accepted),
    );

    // The predecessor left the Run marked running with a Checkpoint on disk.
    let stranded = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("the crashed daemon recorded the run");
    assert_eq!(stranded.state, LocalRunState::Running);

    // A replacement daemon on the same state root must pick it back up.
    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    daemon.recover_unfinished().await.expect("recovery runs");
    tokio::spawn(daemon.serve(listener));

    let probe = state_root.clone();
    wait_for("the recovered run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if !matches!(record.state, LocalRunState::Running)
        )
    })
    .await;

    let recovered = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present");
    assert_eq!(
        recovered.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        },
        "the replacement daemon must finish the stranded Run"
    );
    assert!(
        recovered.owner_epoch > stranded.owner_epoch,
        "recovery must take a strictly newer owner epoch: {} -> {}",
        stranded.owner_epoch,
        recovered.owner_epoch
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_provider_attempt_is_never_replayed_past_its_durable_budget() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_stranding_provider().await;
    let mut bounded = config(state_root.clone(), workspace_root.clone(), endpoint.clone());
    bounded
        .model_routing
        .health_policy
        .max_same_provider_attempts = 1;

    let run_id = crash_after_first_checkpoint(bounded.clone(), Arc::clone(&accepted));
    LocalRuntimeDaemon::new(bounded.clone())
        .recover_unfinished()
        .await
        .expect("first replacement reconciles the interrupted attempt");
    let recovered_root = state_root.clone();
    wait_for(
        "the bounded interrupted attempt to become terminal",
        move || {
            matches!(
                LocalRuntimeHost::read_run_record(&recovered_root, run_id),
                Ok(Some(record)) if !matches!(record.state, LocalRunState::Running)
            )
        },
    )
    .await;
    let recovered = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("recovered record");
    assert!(
        matches!(
            recovered.state,
            LocalRunState::Finished { ref status } if status.contains("exhausted the frozen same-provider attempt budget")
        ),
        "the ambiguous request must terminate explicitly instead of being replayed: {:?}",
        recovered.state
    );

    LocalRuntimeDaemon::new(bounded)
        .recover_unfinished()
        .await
        .expect("later replacement leaves the terminal Run alone");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "no replacement daemon may exceed the persisted Provider attempt budget"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_charges_an_active_crash_gap_and_times_out_without_reinvoking_the_model() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_stranding_provider().await;
    let mut bounded = config(state_root.clone(), workspace_root.clone(), endpoint.clone());
    bounded.budget.max_duration_seconds = 1;

    let run_id = crash_after_first_checkpoint(bounded, Arc::clone(&accepted));
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    let mut replacement = config(state_root.clone(), workspace_root, endpoint);
    replacement.budget.max_duration_seconds = 1;
    LocalRuntimeDaemon::new(replacement)
        .recover_unfinished()
        .await
        .expect("bounded recovery");
    let probe = state_root.clone();
    wait_for("the recovered active Run to terminate", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if !matches!(record.state, LocalRunState::Running)
        )
    })
    .await;

    let recovered = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present");
    assert_eq!(
        recovered.state,
        LocalRunState::Finished {
            status: "timed_out".into()
        },
        "active crash downtime must exhaust the persisted duration budget"
    );

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "an exhausted recovered Run must not open a second model request"
    );
    let events = LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "run.timed_out")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_acknowledged_cancellation_survives_a_daemon_crash_without_reinvoking_the_model() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_stranding_provider().await;

    let run_id = crash_after_cancel_ack(
        config(state_root.clone(), workspace_root.clone(), endpoint.clone()),
        Arc::clone(&accepted),
    );

    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind replacement");
    let replacement = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    let state_before_recovery = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present")
        .state;
    let expected_recovery_count = usize::from(matches!(
        state_before_recovery,
        LocalRunState::Cancelling { .. }
    ));
    assert_eq!(
        replacement
            .recover_unfinished()
            .await
            .expect("recovery runs"),
        expected_recovery_count,
        "only an intent not already closed by the predecessor needs recovery"
    );
    let serving = tokio::spawn(replacement.serve(listener));

    wait_for("the recovered cancellation to become terminal", || {
        matches!(
            LocalRuntimeHost::read_run_record(&state_root, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Cancelled { .. })
        )
    })
    .await;
    let events = LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "run.cancelled")
            .count(),
        1,
        "recovery must durably close the Kernel attempt exactly once"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "a cancellation recovery must not issue another model request"
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_finishes_a_durable_cancellation_intent_without_reinvoking_the_model() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_stranding_provider().await;

    let run_id = crash_after_first_checkpoint(
        config(state_root.clone(), workspace_root.clone(), endpoint.clone()),
        Arc::clone(&accepted),
    );
    let running = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present");
    assert_eq!(running.state, LocalRunState::Running);
    LocalRuntimeHost::write_run_record(
        &state_root,
        &agent_runtime_host::LocalRunRecord {
            state: LocalRunState::Cancelling {
                reason: "cancelled by the local operator".into(),
            },
            ..running
        },
    )
    .expect("persist cancellation intent fixture");

    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind replacement");
    let replacement = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    assert_eq!(
        replacement
            .recover_unfinished()
            .await
            .expect("recovery runs"),
        1
    );
    let serving = tokio::spawn(replacement.serve(listener));
    wait_for("the durable cancellation intent to close", || {
        matches!(
            LocalRuntimeHost::read_run_record(&state_root, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Cancelled { .. })
        )
    })
    .await;

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "cancellation recovery must not issue another model request"
    );
    let events = LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "run.cancelled")
            .count(),
        1
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_reconciles_a_terminal_event_before_a_stale_cancellation_intent() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (endpoint, accepted) = spawn_one_answer_provider().await;
    let local_config = config(state_root.clone(), workspace_root.clone(), endpoint.clone());
    let run_id = Uuid::now_v7();
    let mut host = LocalRuntimeHost::start(local_config.clone()).expect("host");
    let outcome = host
        .execute_as(run_id, "Finish before cancellation is recorded.")
        .await
        .expect("first execution");
    assert_eq!(format!("{:?}", outcome.status).to_lowercase(), "succeeded");
    let terminal_events_before = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type.starts_with("run."))
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "run.succeeded" | "run.failed" | "run.cancelled" | "run.timed_out"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_events_before.len(), 1);
    LocalRuntimeHost::write_run_record(
        &state_root,
        &agent_runtime_host::LocalRunRecord {
            store_version: 1,
            tenant_id: local_invocation_context().tenant_id,
            application_id: local_invocation_context().application_id,
            workload_identity_id: local_invocation_context().workload_identity_id,
            workspace_id: local_invocation_context().workspace_id,
            agent_version_id: local_invocation_context().agent_version_id,
            model_policy_id: local_invocation_context().model_policy_id,
            run_id,
            input: "Finish before cancellation is recorded.".into(),
            state: LocalRunState::Cancelling {
                reason: "cancel raced terminal record persistence".into(),
            },
            owner_epoch: 1,
        },
    )
    .expect("stale cancellation fixture");
    drop(host);

    let replacement = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    assert_eq!(
        replacement.recover_unfinished().await.expect("recovery"),
        0,
        "a durable terminal event must win over a stale cancellation intent"
    );
    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present");
    assert_eq!(
        record.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        }
    );
    let terminal_events_after = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "run.succeeded" | "run.failed" | "run.cancelled" | "run.timed_out"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_events_after, terminal_events_before);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_never_re_executes_a_run_that_already_finished() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    // Exactly one turn is available: a second execution would strand forever.
    let (endpoint, accepted) = spawn_stranding_provider().await;

    // Burn the stranding first connection so the next call succeeds.
    let _ = tokio::net::TcpStream::connect(
        endpoint
            .trim_start_matches("http://")
            .trim_end_matches("/v1/chat/completions"),
    )
    .await;
    // Wait until the provider has actually counted it. Connecting is not the
    // same as being accepted, and if the daemon's own call is accepted first it
    // is the one that gets stranded.
    wait_for("the stranding connection to be consumed", || {
        accepted.load(Ordering::SeqCst) >= 1
    })
    .await;

    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config(
        state_root.clone(),
        workspace_root.clone(),
        endpoint.clone(),
    ));
    tokio::spawn(daemon.serve(listener));
    let run_id = submit(&socket, "Summarize the workspace.").await;

    let probe = state_root.clone();
    wait_for("the run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;
    let finished = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    let events_before =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable");

    // A replacement daemon must leave the finished Run alone.
    let replacement = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    replacement
        .recover_unfinished()
        .await
        .expect("recovery runs");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        LocalRuntimeHost::read_run_record(&state_root, run_id)
            .expect("readable")
            .expect("present"),
        finished,
        "a finished Run must not be touched by recovery"
    );
    assert_eq!(
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable"),
        events_before,
        "recovery must not append events to a finished Run"
    );
}
