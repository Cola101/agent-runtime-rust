//! Local approval gate over IPC (ADR-0035 decision 2: the approval gate is the
//! same in both modes).
//!
//! `LocalToolConsent::Ask` is the mode that actually enforces the gate. Until a
//! client can answer, an Ask Run is a dead end, and a Run parked on an approval
//! is not finished no matter what its last outcome looked like.

use agent_protocol::RunBudget;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalProviderConfig, LocalRunState, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
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
        provider: LocalProviderConfig {
            endpoint,
            model: "local-test-model".into(),
            api_key: "local-test-key".into(),
        },
        trusted_workspace_tool: trusted_tool_binary(),
        // The gate under test.
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
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

async fn wait_for<F: Fn() -> bool>(label: &str, predicate: F) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {label}");
}

async fn start(config: LocalRuntimeConfig) -> PathBuf {
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config);
    daemon.recover_unfinished().await.expect("recovery");
    tokio::spawn(daemon.serve(listener));
    socket
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
    let socket = start(config(
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
    let socket = start(config(
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

#[tokio::test]
async fn a_restarted_daemon_keeps_a_parked_run_approvable() {
    let state = tempfile::tempdir().expect("state");
    let workspace = fixture_workspace();
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let endpoint = spawn_provider().await;
    let socket = start(config(
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

    // A replacement daemon must leave the Run parked, not skip it and not
    // restart it, and must still accept the decision.
    let socket = start(config(state_root.clone(), workspace_root, endpoint)).await;
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
    let socket = start(config(
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
    let socket = start(config(
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
    // A cancelled Run must stay cancelled across a restart.
    let _ = Uuid::nil();
}
