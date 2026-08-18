use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
    RuntimeControlAction, RuntimeControlCommand, RuntimeControlReceipt, RuntimeControlReceiptState,
    RuntimeEventCursorRequest, RuntimeEventCursorState, RuntimeProfile,
};
use agent_runtime_host::{
    LocalApprovalDecision, LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunState,
    LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const TOOL_CALL_TURN: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_control_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace.read_text\",\"arguments\":\"{\\\"path\\\":\\\"README.txt\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
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

fn trusted_tool_binary() -> Option<PathBuf> {
    let mut current = std::env::current_exe().ok()?;
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    endpoint: String,
    trusted_tool: Option<PathBuf>,
) -> LocalRuntimeConfig {
    let mut model_routing =
        LocalModelRoutingConfig::single_openai_compatible(endpoint, "test-model", "test-key");
    model_routing.health_policy.max_same_provider_attempts = 2;
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Use only the registered invocation.".into(),
        delegated_scopes: trusted_tool
            .as_ref()
            .map(|_| BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]))
            .unwrap_or_default(),
        subagent_roles: Vec::new(),
        model_routing,
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: trusted_tool,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 60,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

fn runtime(profile: RuntimeProfile) -> Arc<EmbeddedRuntime> {
    Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 2,
                max_active_runs_per_tenant: 2,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 8,
                max_queued_runs_per_tenant: 8,
            },
            vec![profile],
        )
        .expect("embedded Runtime"),
    )
}

async fn write_response(socket: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.expect("reply");
}

async fn spawn_approval_provider() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
        for body in [
            TOOL_CALL_TURN.to_owned(),
            text_turn("approved and finished"),
        ] {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            observed.fetch_add(1, Ordering::SeqCst);
            write_response(&mut socket, &body).await;
        }
    });
    (endpoint, calls)
}

async fn spawn_cancellable_provider() -> (String, tokio::sync::oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let mut request = vec![0_u8; 64 * 1024];
        let _ = socket.read(&mut request).await;
        let _ = seen_tx.send(());
        let mut sink = [0_u8; 1];
        let _ = socket.read(&mut sink).await;
    });
    (endpoint, seen_rx)
}

/// Holds the first call open, then answers the second and reports what it was
/// asked.
///
/// The held call is the whole point: a steer that arrives while nothing is in
/// flight proves nothing about interrupting one. The second request body is
/// returned because the claim being tested is not "the Run continued" but
/// "the Run continued *with what the person added*".
async fn spawn_steerable_provider() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Receiver<String>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut held, _) = listener.accept().await.expect("first request");
        let mut request = vec![0_u8; 64 * 1024];
        let read = held.read(&mut request).await.unwrap_or(0);
        let _ = read;
        let _ = seen_tx.send(());

        let (mut socket, _) = listener.accept().await.expect("second request");
        let mut body = vec![0_u8; 256 * 1024];
        let read = socket.read(&mut body).await.unwrap_or(0);
        let _ = second_tx.send(String::from_utf8_lossy(&body[..read]).into_owned());
        write_response(&mut socket, &text_turn("redirected and finished")).await;
        // Held until the run is over, so the first call never completes.
        let mut sink = [0_u8; 1];
        let _ = held.read(&mut sink).await;
    });
    (endpoint, seen_rx, second_rx)
}

async fn spawn_recoverable_provider() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    Arc<AtomicUsize>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first request");
        let mut request = vec![0_u8; 64 * 1024];
        let _ = first.read(&mut request).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let _ = seen_tx.send(());
        let _ = release_rx.await;
        drop(first);

        let (mut second, _) = listener.accept().await.expect("recovery request");
        let _ = second.read(&mut request).await;
        observed.fetch_add(1, Ordering::SeqCst);
        write_response(&mut second, &text_turn("recovered exactly once")).await;
    });
    (endpoint, seen_rx, release_tx, calls)
}

async fn spawn_double_crash_provider() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    Arc<AtomicUsize>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
    let (second_seen_tx, second_seen_rx) = tokio::sync::oneshot::channel();
    let (release_second_tx, release_second_rx) = tokio::sync::oneshot::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tokio::spawn(async move {
        let mut request = vec![0_u8; 64 * 1024];
        let (mut first, _) = listener.accept().await.expect("first request");
        let _ = first.read(&mut request).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let _ = first_seen_tx.send(());
        let _ = release_first_rx.await;
        drop(first);

        let (mut second, _) = listener.accept().await.expect("second request");
        let _ = second.read(&mut request).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let _ = second_seen_tx.send(());
        let _ = release_second_rx.await;
        drop(second);

        let (mut third, _) = listener.accept().await.expect("third request");
        let _ = third.read(&mut request).await;
        observed.fetch_add(1, Ordering::SeqCst);
        write_response(&mut third, &text_turn("recovered accepted command")).await;
    });
    (
        endpoint,
        first_seen_rx,
        release_first_tx,
        second_seen_rx,
        release_second_tx,
        calls,
    )
}

async fn wait_for_record(
    runtime: &EmbeddedRuntime,
    identity: RuntimeInvocationContext,
    run_id: Uuid,
    predicate: impl Fn(&LocalRunState) -> bool,
) -> agent_runtime_host::LocalRunRecord {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(record) = runtime
            .read_run_record(identity, run_id)
            .expect("read Run record")
            && predicate(&record.state)
        {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Run record"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn exact_approval_command_is_durable_idempotent_and_executes_the_tool_once() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "approval evidence\n").unwrap();
    let identity = invocation();
    let (endpoint, provider_calls) = spawn_approval_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            Some(trusted_tool),
        ),
    });
    let run_id = Uuid::now_v7();

    let parked = runtime
        .execute(identity, run_id, "Read README.txt")
        .await
        .expect("park on approval");
    let approval = parked.pending_approval.expect("pending approval");
    let record = runtime
        .read_run_record(identity, run_id)
        .expect("record read")
        .expect("durable run record");
    assert!(matches!(
        record.state,
        LocalRunState::AwaitingApproval { .. }
    ));
    let approval_page = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id,
            after_sequence: 0,
            limit: 256,
        })
        .expect("waiting approval cursor");
    assert_eq!(
        approval_page.state,
        RuntimeEventCursorState::WaitingApproval
    );
    assert!(!approval_page.has_more);
    assert!(!approval_page.history_gap);

    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::DecideApproval {
            target_run_id: approval.target_run_id,
            approval_id: approval.approval_id,
            binding_digest: approval.binding_digest,
            decision: LocalApprovalDecision::AllowOnce,
        },
    };
    let applied = runtime.control(command.clone()).await.expect("approve");
    assert_eq!(applied.receipt.state, RuntimeControlReceiptState::Completed);
    assert_eq!(applied.receipt.run_status, Some(RunStatus::Succeeded));
    assert_eq!(
        applied
            .outcome
            .as_ref()
            .map(|outcome| outcome.output.as_str()),
        Some("approved and finished")
    );

    let repeated = runtime.control(command).await.expect("idempotent replay");
    assert_eq!(repeated.receipt, applied.receipt);
    assert!(
        repeated.outcome.is_none(),
        "a completed receipt must not replay the Run"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    let types = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id,
            after_sequence: 0,
            limit: 256,
        })
        .expect("events")
        .events
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        types
            .iter()
            .filter(|event| *event == "tool.execution.started")
            .count(),
        1
    );
}

#[tokio::test]
async fn accepted_approval_storage_failure_keeps_recoverable_state_without_fake_terminal() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "approval evidence\n").unwrap();
    let identity = invocation();
    let (endpoint, _) = spawn_approval_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            Some(trusted_tool),
        ),
    });
    let run_id = Uuid::now_v7();
    let parked = runtime
        .execute(identity, run_id, "Read README.txt")
        .await
        .expect("park on approval");
    let approval = parked.pending_approval.expect("pending approval");
    let record = runtime
        .read_run_record(identity, run_id)
        .expect("record read")
        .expect("durable Run record");

    // Force the restored event append to fail only after the command has
    // passed its Checkpoint preflight and obtained a durable receipt.
    let event_log = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl");
    std::fs::remove_file(&event_log).expect("remove test event log");
    std::fs::create_dir(&event_log).expect("replace event log with a directory");
    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::DecideApproval {
            target_run_id: approval.target_run_id,
            approval_id: approval.approval_id,
            binding_digest: approval.binding_digest,
            decision: LocalApprovalDecision::AllowOnce,
        },
    };
    let accepted = runtime
        .control_detached(command.clone())
        .await
        .expect("durably accepted command");
    assert_eq!(accepted.state, RuntimeControlReceiptState::Accepted);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = wait_for_record(&runtime, identity, run_id, |state| {
        matches!(
            state,
            LocalRunState::ApprovalDecided { .. } | LocalRunState::Finished { .. }
        )
    })
    .await;
    assert!(
        matches!(after.state, LocalRunState::ApprovalDecided { .. }),
        "an adapter/storage failure must leave the accepted decision recoverable, not invent a Run failure: {:?}",
        after.state
    );
    let receipt = runtime
        .read_control_receipt(identity, command.command_id)
        .expect("receipt read")
        .expect("receipt retained");
    assert_eq!(receipt.state, RuntimeControlReceiptState::Accepted);
    assert_eq!(receipt.run_status, None);
}

#[tokio::test]
async fn cancelling_an_active_embedded_run_persists_intent_and_wakes_the_owner() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, request_seen) = spawn_cancellable_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let run_id = Uuid::now_v7();
    let executing_runtime = Arc::clone(&runtime);
    let execution = tokio::spawn(async move {
        executing_runtime
            .execute(identity, run_id, "wait for cancellation")
            .await
    });
    request_seen.await.expect("provider request");
    let record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::Cancel {
            reason: "cancelled by the embedding application".into(),
        },
    };

    let accepted = runtime.control(command.clone()).await.expect("cancel");
    assert_eq!(accepted.receipt.state, RuntimeControlReceiptState::Accepted);
    let outcome = execution.await.unwrap().expect("cancelled outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    let repeated = runtime
        .control(command)
        .await
        .expect("read completed cancellation");
    assert_eq!(
        repeated.receipt.state,
        RuntimeControlReceiptState::Completed
    );
    assert_eq!(repeated.receipt.run_status, Some(RunStatus::Cancelled));
    assert!(repeated.outcome.is_none());
}

#[tokio::test]
async fn resume_command_recovers_a_dropped_embedded_owner_without_replaying_its_command() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, first_seen, release_first, calls) = spawn_recoverable_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let run_id = Uuid::now_v7();
    let first_runtime = Arc::clone(&runtime);
    let first = tokio::spawn(async move {
        first_runtime
            .execute(identity, run_id, "recover this Run")
            .await
    });
    first_seen.await.expect("first request");
    let record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    let checkpoint = LocalRuntimeHost::checkpoint_path(state.path(), run_id);
    assert!(
        checkpoint.is_file(),
        "accepted Run must checkpoint before model egress"
    );
    first.abort();
    let _ = first.await;
    release_first.send(()).ok();

    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::Resume,
    };
    let recovered = runtime.control(command.clone()).await.expect("resume");
    assert_eq!(
        recovered.receipt.state,
        RuntimeControlReceiptState::Completed
    );
    assert_eq!(recovered.receipt.run_status, Some(RunStatus::Succeeded));
    assert_eq!(
        recovered
            .outcome
            .as_ref()
            .map(|outcome| outcome.output.as_str()),
        Some("recovered exactly once")
    );
    let repeated = runtime.control(command).await.expect("idempotent receipt");
    assert_eq!(repeated.receipt, recovered.receipt);
    assert!(repeated.outcome.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn control_rejects_stale_epoch_wrong_binding_and_command_id_reuse_before_mutation() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "contract evidence\n").unwrap();
    let identity = invocation();
    let (endpoint, provider_calls) = spawn_approval_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            Some(trusted_tool),
        ),
    });
    let run_id = Uuid::now_v7();
    let parked = runtime
        .execute(identity, run_id, "Read README.txt")
        .await
        .expect("park on approval");
    let approval = parked.pending_approval.expect("pending approval");
    let record = runtime
        .read_run_record(identity, run_id)
        .expect("record read")
        .expect("durable Run record");
    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::DecideApproval {
            target_run_id: approval.target_run_id,
            approval_id: approval.approval_id,
            binding_digest: approval.binding_digest.clone(),
            decision: LocalApprovalDecision::AllowOnce,
        },
    };

    let mut stale = command.clone();
    stale.expected_owner_epoch += 1;
    assert!(runtime.control(stale).await.is_err());
    assert!(matches!(
        runtime
            .read_run_record(identity, run_id)
            .unwrap()
            .unwrap()
            .state,
        LocalRunState::AwaitingApproval { .. }
    ));

    let mut wrong_binding = command.clone();
    wrong_binding.action = RuntimeControlAction::DecideApproval {
        target_run_id: approval.target_run_id,
        approval_id: approval.approval_id,
        binding_digest: "0".repeat(64),
        decision: LocalApprovalDecision::AllowOnce,
    };
    assert!(runtime.control(wrong_binding).await.is_err());
    assert!(matches!(
        runtime
            .read_run_record(identity, run_id)
            .unwrap()
            .unwrap()
            .state,
        LocalRunState::AwaitingApproval { .. }
    ));

    let applied = runtime.control(command.clone()).await.expect("approve");
    assert_eq!(applied.receipt.state, RuntimeControlReceiptState::Completed);

    let mut rebound = command;
    rebound.action = RuntimeControlAction::Cancel {
        reason: "reuse the id for another command".into(),
    };
    assert!(runtime.control(rebound).await.is_err());
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn local_run_record_reads_fail_closed_on_storage_errors() {
    let state = tempfile::tempdir().expect("state");
    let run_id = Uuid::now_v7();
    std::fs::create_dir_all(
        state
            .path()
            .join("runs")
            .join(run_id.to_string())
            .join("run.json"),
    )
    .expect("make run.json unreadable as a regular file");

    assert!(matches!(
        LocalRuntimeHost::read_run_record(state.path(), run_id),
        Err(agent_runtime_host::LocalRuntimeError::StateRoot(_))
    ));
}

#[tokio::test]
async fn concurrent_approval_commands_have_one_owner_and_one_durable_receipt() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "single owner\n").unwrap();
    let identity = invocation();
    let (endpoint, provider_calls) = spawn_approval_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            Some(trusted_tool),
        ),
    });
    let run_id = Uuid::now_v7();
    let parked = runtime
        .execute(identity, run_id, "Read README.txt")
        .await
        .expect("park on approval");
    let approval = parked.pending_approval.expect("pending approval");
    let record = runtime
        .read_run_record(identity, run_id)
        .unwrap()
        .expect("durable Run record");
    let command = |command_id| RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id,
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::DecideApproval {
            target_run_id: approval.target_run_id,
            approval_id: approval.approval_id,
            binding_digest: approval.binding_digest.clone(),
            decision: LocalApprovalDecision::AllowOnce,
        },
    };
    let first_command = command(Uuid::now_v7());
    let second_command = command(Uuid::now_v7());
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_runtime = Arc::clone(&runtime);
    let first_barrier = Arc::clone(&barrier);
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_runtime.control(first_command).await
    });
    let second_runtime = Arc::clone(&runtime);
    let second_barrier = Arc::clone(&barrier);
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_runtime.control(second_command).await
    });
    barrier.wait().await;
    let (first, second) = tokio::join!(first, second);
    let results = [first.unwrap(), second.unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    let receipts = std::fs::read_dir(state.path().join("control-receipts"))
        .expect("receipt directory")
        .count();
    assert_eq!(receipts, 1, "the rejected command must not be accepted");
    let tool_starts = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id,
            after_sequence: 0,
            limit: 256,
        })
        .unwrap()
        .events
        .iter()
        .filter(|event| event.event_type == "tool.execution.started")
        .count();
    assert_eq!(tool_starts, 1);
}

#[tokio::test]
async fn concurrent_cancellations_leave_no_accepted_receipt_orphaned() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, request_seen) = spawn_cancellable_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let run_id = Uuid::now_v7();
    let executing_runtime = Arc::clone(&runtime);
    let execution = tokio::spawn(async move {
        executing_runtime
            .execute(identity, run_id, "wait for concurrent cancellation")
            .await
    });
    request_seen.await.expect("provider request");
    let record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    let command = |reason: &str| RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::Cancel {
            reason: reason.into(),
        },
    };
    let commands = [
        command("first cancellation"),
        command("second cancellation"),
    ];
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_runtime = Arc::clone(&runtime);
    let first_barrier = Arc::clone(&barrier);
    let first_command = commands[0].clone();
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_runtime.control(first_command).await
    });
    let second_runtime = Arc::clone(&runtime);
    let second_barrier = Arc::clone(&barrier);
    let second_command = commands[1].clone();
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_runtime.control(second_command).await
    });
    barrier.wait().await;
    let (first, second) = tokio::join!(first, second);
    let results = [first.unwrap(), second.unwrap()];
    assert!(results.iter().any(Result::is_ok));

    let outcome = execution.await.unwrap().expect("cancelled outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    for (result, command) in results.into_iter().zip(commands) {
        if result.is_ok() {
            let replayed = runtime
                .control(command)
                .await
                .expect("accepted cancellation must remain queryable");
            assert_eq!(
                replayed.receipt.state,
                RuntimeControlReceiptState::Completed
            );
            assert_eq!(replayed.receipt.run_status, Some(RunStatus::Cancelled));
        }
    }
}

#[tokio::test]
async fn an_accepted_resume_command_survives_a_second_owner_crash() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, first_seen, release_first, second_seen, release_second, calls) =
        spawn_double_crash_provider().await;
    let mut runtime_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        None,
    );
    runtime_config
        .model_routing
        .health_policy
        .max_same_provider_attempts = 3;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: runtime_config,
    });
    let run_id = Uuid::now_v7();
    let first_runtime = Arc::clone(&runtime);
    let first = tokio::spawn(async move {
        first_runtime
            .execute(identity, run_id, "survive two owners")
            .await
    });
    first_seen.await.expect("first provider request");
    let first_record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    first.abort();
    let _ = first.await;
    release_first.send(()).ok();

    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: first_record.owner_epoch,
        action: RuntimeControlAction::Resume,
    };
    let recovering_runtime = Arc::clone(&runtime);
    let recovering_command = command.clone();
    let recovery =
        tokio::spawn(async move { recovering_runtime.control(recovering_command).await });
    second_seen.await.expect("second provider request");
    let receipt_path = state
        .path()
        .join("control-receipts")
        .join(format!("{}.json", command.command_id));
    let receipt: RuntimeControlReceipt =
        serde_json::from_slice(&std::fs::read(receipt_path).expect("accepted receipt"))
            .expect("receipt JSON");
    assert_eq!(receipt.state, RuntimeControlReceiptState::Accepted);
    recovery.abort();
    let _ = recovery.await;
    release_second.send(()).ok();

    let recovered = runtime
        .control(command.clone())
        .await
        .expect("resume accepted command");
    assert_eq!(
        recovered.receipt.state,
        RuntimeControlReceiptState::Completed
    );
    assert_eq!(recovered.receipt.run_status, Some(RunStatus::Succeeded));
    assert_eq!(
        recovered
            .outcome
            .as_ref()
            .map(|outcome| outcome.output.as_str()),
        Some("recovered accepted command")
    );
    let replayed = runtime.control(command).await.expect("receipt replay");
    assert_eq!(replayed.receipt, recovered.receipt);
    assert!(replayed.outcome.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// The claim: a steer sent while the model is mid-call interrupts that call and
/// the Run continues with what the person added.
///
/// Everything about this test is chosen so it cannot pass for another reason.
/// The provider holds the first call open forever, so nothing but an
/// interruption can end it. The second request body is inspected, so
/// "continued" is not mistaken for "continued with the steering". And the Run
/// is asserted to succeed, because a steer that ends a Run has cancelled it
/// under another name.
#[tokio::test]
async fn steering_an_active_run_interrupts_the_model_call_and_redirects_it() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, request_seen, second_request) = spawn_steerable_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let run_id = Uuid::now_v7();
    let executing_runtime = Arc::clone(&runtime);
    let execution = tokio::spawn(async move {
        executing_runtime
            .execute(identity, run_id, "start something long")
            .await
    });
    request_seen.await.expect("provider request");
    let record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;

    let accepted = runtime
        .control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: identity,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Steer {
                steering_id: Uuid::now_v7(),
                input: "actually look at the retention sweep instead".into(),
            },
        })
        .await
        .expect("steer");
    assert_eq!(accepted.receipt.state, RuntimeControlReceiptState::Accepted);

    let asked = tokio::time::timeout(Duration::from_secs(20), second_request)
        .await
        .expect("the steer did not interrupt the model call within twenty seconds")
        .expect("second request");
    assert!(
        asked.contains("actually look at the retention sweep instead"),
        "the turn was asked again without what the person added: {asked}"
    );

    let outcome = execution.await.unwrap().expect("steered outcome");
    // A steer redirects a Run. One that ends it has cancelled it under another
    // name.
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "run.steer.applied"),
        "steering left no durable evidence: {:?}",
        outcome.event_types
    );
}

/// A steer for a Run that is not moving is refused rather than accepted and
/// dropped.
#[tokio::test]
async fn steering_a_run_that_is_not_executing_here_is_refused() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, _calls) = spawn_approval_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let refused = runtime
        .control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: identity,
            run_id: Uuid::now_v7(),
            expected_owner_epoch: 1,
            action: RuntimeControlAction::Steer {
                steering_id: Uuid::now_v7(),
                input: "nobody is listening".into(),
            },
        })
        .await;
    assert!(matches!(
        refused,
        Err(agent_runtime_host::embedded::EmbeddedRuntimeError::NotSteerable)
    ));
}

/// The regression the design predicted before it was written.
///
/// `apply_steering` installs a token with no parent. This Host cancels a Run by
/// cancelling the root of a tree, so an attempt left holding an unparented
/// token has quietly left that tree -- and Cancel would stop working for it,
/// discoverable only by someone cancelling a Run they had steered.
#[tokio::test]
async fn a_steered_run_can_still_be_cancelled() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation();
    let (endpoint, request_seen, second_request) = spawn_steerable_provider().await;
    let runtime = runtime(RuntimeProfile {
        invocation: identity,
        config: config(
            state.path().to_path_buf(),
            workspace.path().canonicalize().unwrap(),
            endpoint,
            None,
        ),
    });
    let run_id = Uuid::now_v7();
    let executing_runtime = Arc::clone(&runtime);
    let execution = tokio::spawn(async move {
        executing_runtime
            .execute(identity, run_id, "start something long")
            .await
    });
    request_seen.await.expect("provider request");
    let record = wait_for_record(&runtime, identity, run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    runtime
        .control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: identity,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Steer {
                steering_id: Uuid::now_v7(),
                input: "go the other way".into(),
            },
        })
        .await
        .expect("steer");
    // The steer has landed once the redirected turn has been asked.
    let _asked = tokio::time::timeout(Duration::from_secs(20), second_request)
        .await
        .expect("the steer did not interrupt the model call")
        .expect("second request");

    let cancelled = tokio::time::timeout(
        Duration::from_secs(20),
        runtime.control(RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: identity,
            run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Cancel {
                reason: "cancelled after steering".into(),
            },
        }),
    )
    .await
    .expect("cancelling a steered Run hung -- the attempt left the cancellation tree")
    .expect("cancel");
    assert!(matches!(
        cancelled.receipt.state,
        RuntimeControlReceiptState::Accepted | RuntimeControlReceiptState::Completed
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(20), execution)
        .await
        .expect("the steered Run never stopped after being cancelled")
        .unwrap()
        .expect("outcome");
    assert_eq!(outcome.status, RunStatus::Cancelled);
}
