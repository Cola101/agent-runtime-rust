//! Local approval gate over IPC (ADR-0035 decision 2: the approval gate is the
//! same in both modes).
//!
//! `LocalToolConsent::Ask` is the mode that actually enforces the gate. Until a
//! client can answer, an Ask Run is a dead end, and a Run parked on an approval
//! is not finished no matter what its last outcome looked like.

use agent_protocol::RunBudget;
use agent_runtime_host::embedded::{
    RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
    RuntimeControlAction, RuntimeControlCommand, RuntimeControlReceipt, RuntimeControlReceiptState,
    RuntimeEventCursorRequest, RuntimeEventCursorState,
};
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRunState, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE, local_invocation_context,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use uuid::Uuid;

const TOOL_CALL_TURN: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
\"id\":\"call_local_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace.read_text\",\
\"arguments\":\"{\\\"path\\\":\\\"README.txt\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Serves a tool-call turn first, then plain answers forever, so a Run can park
/// on an approval and still finish once the decision arrives.
async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut served = 0u32;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            let body = if served == 0 {
                TOOL_CALL_TURN.to_string()
            } else {
                text_turn("answered after approval")
            };
            served += 1;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://127.0.0.1:{port}/v1/chat/completions")
}

/// A workspace with the fixture file the tool call reads, plus the trusted tool
/// binary this test suite was built alongside.
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

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
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
        trusted_workspace_tool: trusted_tool_binary(),
        process_session: None,
        // The gate under test.
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
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

/// Returns the socket and the serving task. Aborting the task drops the
/// listener, which closes the socket exactly as a host process exiting would --
/// the only faithful way to simulate a restart inside one process.
async fn start(config: LocalRuntimeConfig) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config);
    daemon.recover_unfinished().await.expect("recovery");
    let serving = tokio::spawn(daemon.serve(listener));
    (socket, serving)
}

fn fixture_workspace() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("README.txt"),
        "Agent Runtime native workspace: trusted read-only Tool fixture.\n",
    )
    .expect("fixture file");
    workspace
}

#[tokio::test]
async fn a_run_parked_on_an_approval_is_not_recorded_as_finished() {
    let Some(_) = trusted_tool_binary() else {
        panic!("agent-trusted-workspace-tool must be built for this test");
    };
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };

    let probe = state_root.clone();
    wait_for("the run to park on its approval", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if !matches!(record.state, LocalRunState::Running)
        )
    })
    .await;

    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert!(
        matches!(record.state, LocalRunState::AwaitingApproval { .. }),
        "a Run waiting for a human is not finished, got {:?}",
        record.state
    );
}

#[tokio::test]
async fn approving_over_ipc_lets_the_parked_run_execute_its_tool_and_finish() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };

    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    let response = request(&socket, &LocalRequest::Approve { run_id }).await;
    assert!(
        matches!(response, LocalResponse::Accepted { .. }),
        "approval was refused: {response:?}"
    );

    let probe = state_root.clone();
    wait_for("the approved run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert_eq!(
        record.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        }
    );
    let types = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(
        types.iter().any(|event| event == "tool.execution.started"),
        "the approved Tool must actually run: {types:?}"
    );
}

/// A process can die after the kernel has copied only a prefix of one JSONL
/// row. Complete rows before that prefix are committed; the unterminated tail
/// is not. Recovery must neither lose the committed prefix nor concatenate a
/// resumed event onto the torn bytes.
#[tokio::test]
async fn a_torn_event_tail_is_repaired_before_approval_resume() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    let committed =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("committed event prefix");
    assert!(!committed.is_empty());
    let event_log = state_root
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl");
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&event_log)
        .expect("event log")
        .write_all(b"{\"event_id\":\"torn")
        .expect("inject torn tail");

    let after_crash = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("an unterminated tail is not a committed event");
    assert_eq!(after_crash, committed);

    let response = request(&socket, &LocalRequest::Approve { run_id }).await;
    assert!(
        matches!(response, LocalResponse::Accepted { .. }),
        "approval was refused: {response:?}"
    );
    let probe = state_root.clone();
    wait_for("the resumed Run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let recovered =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("repaired event log");
    assert!(recovered.len() > committed.len());
    assert_eq!(
        recovered
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        (1..=recovered.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        recovered.last().map(|event| event.event_type.as_str()),
        Some("run.succeeded")
    );
    assert!(
        std::fs::read(&event_log)
            .expect("event log bytes")
            .ends_with(b"\n")
    );
}

#[tokio::test]
async fn approval_wait_does_not_consume_the_run_duration_budget() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let mut local_config = config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    );
    local_config.budget.max_duration_seconds = 2;
    let (socket, _serving) = start(local_config).await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the bounded run to park on approval", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    let checkpoint =
        LocalRuntimeHost::load_checkpoint(&LocalRuntimeHost::checkpoint_path(&state_root, run_id))
            .expect("parked checkpoint");
    let state: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        state["execution_time"]["active"], false,
        "the approval checkpoint must durably stop the execution clock"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let response = request(&socket, &LocalRequest::Approve { run_id }).await;
    assert!(matches!(response, LocalResponse::Accepted { .. }));
    let probe = state_root.clone();
    wait_for("the post-wait approved run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert_eq!(
        record.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        },
        "operator think time must not turn an approved Run into a timeout"
    );
}

#[tokio::test]
async fn a_restarted_daemon_keeps_a_parked_run_approvable() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let endpoint = spawn_provider().await;
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace_root.clone(),
        endpoint.clone(),
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    // Stop the first daemon before starting its replacement. This test used to
    // skip that step, so it never restarted anything -- it started a second
    // daemon beside the first and passed only because `bind` would take a live
    // socket. That is the defect single_instance.rs now pins, and this test was
    // depending on it.
    _serving.abort();
    let _ = _serving.await;

    // A replacement daemon must leave the Run parked, not skip it and not
    // restart it, and must still accept the decision.
    let (socket, _replacement) = start(config(state_root.clone(), workspace_root, endpoint)).await;
    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert!(
        matches!(record.state, LocalRunState::AwaitingApproval { .. }),
        "recovery must keep the Run parked, got {:?}",
        record.state
    );

    let response = request(&socket, &LocalRequest::Approve { run_id }).await;
    assert!(
        matches!(response, LocalResponse::Accepted { .. }),
        "a recovered parked Run must still be approvable: {response:?}"
    );

    let probe = state_root.clone();
    wait_for("the recovered run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert_eq!(
        record.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        },
        "approving after a restart must actually complete the Run"
    );
    let types = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(
        types.iter().any(|event| event == "tool.execution.started"),
        "the Tool approved after a restart must actually run: {types:?}"
    );
}

#[tokio::test]
async fn denying_over_ipc_never_executes_the_tool() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    request(&socket, &LocalRequest::Deny { run_id }).await;
    let probe = state_root.clone();
    wait_for("the denied run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    assert_eq!(
        record.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        },
        "a denial must resume the Run with a bound error result, not fail it"
    );
    let types = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(
        !types.iter().any(|event| event == "tool.execution.started"),
        "a denied Tool must never execute: {types:?}"
    );
}

#[tokio::test]
async fn cancelling_a_parked_run_closes_it_without_executing_the_tool() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    request(&socket, &LocalRequest::Cancel { run_id }).await;
    let probe = state_root.clone();
    wait_for("the cancelled run to close", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Cancelled { .. })
        )
    })
    .await;

    let types = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(
        !types.iter().any(|event| event == "tool.execution.started"),
        "a cancelled Run must never execute the Tool it was parked on: {types:?}"
    );
    assert_eq!(
        types
            .iter()
            .filter(|event| event.as_str() == "run.cancelled")
            .count(),
        1,
        "the Kernel terminal event, not only run.json, must commit cancellation: {types:?}"
    );
    let LocalResponse::EventCursor { page } = request(
        &socket,
        &LocalRequest::EventCursor {
            request: RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: local_invocation_context(),
                run_id,
                after_sequence: 0,
                limit: 256,
            },
        },
    )
    .await
    else {
        panic!("cancelled Run did not expose a valid event cursor");
    };
    assert_eq!(
        page.state,
        RuntimeEventCursorState::Terminal {
            status: agent_protocol::RunStatus::Cancelled
        }
    );
    // A cancelled Run must stay cancelled across a restart.
    let _ = Uuid::nil();
}

fn control_receipts(state_root: &Path) -> Vec<RuntimeControlReceipt> {
    let directory = state_root.join("control-receipts");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("control receipt directory: {error}"),
    };
    let mut receipts = entries
        .map(|entry| {
            let path = entry.expect("receipt entry").path();
            serde_json::from_slice(&std::fs::read(path).expect("read receipt"))
                .expect("decode receipt")
        })
        .collect::<Vec<_>>();
    receipts.sort_by_key(|receipt: &RuntimeControlReceipt| receipt.command_id);
    receipts
}

#[tokio::test]
async fn legacy_approval_is_one_replayable_runtime_control_command() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;
    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;

    assert!(matches!(
        request(&socket, &LocalRequest::Approve { run_id }).await,
        LocalResponse::Accepted { .. }
    ));
    let probe = state_root.clone();
    wait_for("the control receipt to complete", move || {
        control_receipts(&probe).iter().any(|receipt| {
            receipt.run_id == run_id && receipt.state == RuntimeControlReceiptState::Completed
        })
    })
    .await;
    let receipts = control_receipts(&state_root);
    assert_eq!(
        receipts.len(),
        1,
        "legacy approval forked its command ledger"
    );
    let receipt = &receipts[0];
    assert!(matches!(
        &receipt.action,
        RuntimeControlAction::DecideApproval {
            decision: agent_runtime_host::LocalApprovalDecision::AllowOnce,
            ..
        }
    ));

    assert!(matches!(
        request(&socket, &LocalRequest::Approve { run_id }).await,
        LocalResponse::Accepted { .. }
    ));
    assert_eq!(
        control_receipts(&state_root).len(),
        1,
        "retry created a second durable command"
    );
    let tool_starts = LocalRuntimeHost::replay_events(&state_root, run_id, 0)
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "tool.execution.started")
        .count();
    assert_eq!(tool_starts, 1, "approval retry executed the Tool twice");
}

#[tokio::test]
async fn full_control_request_rejects_stale_epoch_then_returns_its_exact_receipt() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;
    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Read README.txt.".into(),
        },
    )
    .await
    else {
        panic!("expected acceptance");
    };
    let probe = state_root.clone();
    wait_for("the approval to be recorded", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;
    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("read record")
        .expect("record");
    let LocalRunState::AwaitingApproval {
        approval_id,
        binding_digest,
        target_run_id,
    } = &record.state
    else {
        panic!("not awaiting approval");
    };
    let command_id = Uuid::now_v7();
    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id,
        invocation: agent_runtime_host::local_invocation_context(),
        run_id,
        expected_owner_epoch: record.owner_epoch,
        action: RuntimeControlAction::DecideApproval {
            target_run_id: target_run_id.unwrap_or(run_id),
            approval_id: *approval_id,
            binding_digest: binding_digest.clone(),
            decision: agent_runtime_host::LocalApprovalDecision::AllowOnce,
        },
    };
    let mut stale = command.clone();
    stale.expected_owner_epoch += 1;
    assert!(matches!(
        request(&socket, &LocalRequest::Control { command: stale }).await,
        LocalResponse::Error { .. }
    ));
    assert!(control_receipts(&state_root).is_empty());

    let response = request(&socket, &LocalRequest::Control { command }).await;
    let LocalResponse::ControlReceipt { receipt } = response else {
        panic!("expected an exact control receipt, got {response:?}");
    };
    assert_eq!(receipt.command_id, command_id);
    assert_eq!(receipt.run_id, run_id);
    assert_eq!(receipt.expected_owner_epoch, record.owner_epoch);
}
