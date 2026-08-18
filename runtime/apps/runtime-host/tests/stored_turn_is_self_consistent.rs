//! Every Tool call in a stored Turn has its result stored beside it.
//!
//! Why this is worth a test of its own: the next Run's prompt is assembled from
//! two sources (`runtime/apps/worker/src/lib.rs`, where the transcript is
//! built) -- an imported history, which is repaired by
//! `repair_imported_history`, and the Session branch's own frozen Turns, which
//! are not repaired by anything. A provider rejects a transcript containing a
//! Tool call with no matching result, so a dangling call in a stored Turn would
//! make a conversation unsendable from then on.
//!
//! Codex does not rely on the store being clean: `ensure_call_outputs_present`
//! (`codex-rs/core/src/context_manager/normalize.rs:20-130`) synthesises an
//! "aborted" result for every dangling call on **every** prompt build
//! (`history.rs:143`). We rely on the store instead -- only a succeeded Turn is
//! committed, and a succeeded Turn's transcript comes from a Checkpoint. That
//! is a claim about our own code, and this is the test that holds it up.
use agent_protocol::{ContentPart, RunBudget};
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, OwnerResponse, default_socket_path,
};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRunState, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};

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

/// A Tool call first, then plain answers, so the Turn parks on an approval and
/// still finishes once the decision arrives.
async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut served = 0_u32;
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            let body = if served == 0 {
                TOOL_CALL_TURN.to_string()
            } else {
                text_turn("answered")
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

fn trusted_tool_binary() -> PathBuf {
    let mut current = std::env::current_exe().expect("test executable path");
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("agent-trusted-workspace-tool is not built; no Tool can be installed")
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Answer briefly.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            endpoint,
            "local-test-model",
            "local-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: Some(trusted_tool_binary()),
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 8_192,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    }
}

fn workspace_with_a_readable_file() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.txt"), "a line\n").expect("seed");
    workspace
}

async fn round_trip(socket: &Path, line: &str) -> String {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    writer.flush().await.expect("flush");
    BufReader::new(reader)
        .lines()
        .next_line()
        .await
        .expect("read")
        .expect("a reply")
}

async fn owner(socket: &Path, request: serde_json::Value) -> OwnerResponse {
    let line = serde_json::to_string(&request).expect("encode");
    serde_json::from_str(&round_trip(socket, &line).await).expect("decode")
}

async fn workload(socket: &Path, request: &LocalRequest) -> LocalResponse {
    let line = serde_json::to_string(request).expect("encode");
    serde_json::from_str(&round_trip(socket, &line).await).expect("decode")
}

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

/// Every Tool call id in the Turn, and every Tool result id, so the two can be
/// compared as sets rather than by position.
fn call_and_result_ids(
    turn: &agent_protocol::SessionConversationTurn,
) -> (Vec<String>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for message in &turn.transcript {
        for part in &message.content {
            match part {
                ContentPart::ToolCall { tool_call_id, .. } => calls.push(tool_call_id.clone()),
                ContentPart::ToolResult { tool_call_id, .. } => results.push(tool_call_id.clone()),
                _ => {}
            }
        }
    }
    (calls, results)
}

async fn stored_turns(
    socket: &Path,
    session_id: uuid::Uuid,
    branch_id: uuid::Uuid,
    generation: u64,
) -> Vec<agent_protocol::SessionConversationTurn> {
    let history = owner(
        socket,
        serde_json::json!({
            "scope": "owner", "type": "session_history",
            "session_id": session_id, "branch_id": branch_id,
            "generation": generation,
        }),
    )
    .await;
    match history {
        OwnerResponse::SessionHistory { page } => page.turns,
        other => panic!("expected a history page, got {other:?}"),
    }
}

async fn start(config: LocalRuntimeConfig) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config);
    daemon.recover_unfinished().await.expect("recovery");
    let serving = tokio::spawn(daemon.serve(listener));
    (socket, serving)
}

/// A Turn whose Tool ran: the call and its result are both stored.
#[tokio::test]
async fn an_executed_tool_leaves_its_result_in_the_stored_turn() {
    let state = tempfile::tempdir().expect("state");
    let workspace = workspace_with_a_readable_file();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let session_id = uuid::Uuid::now_v7();
    let branch_id = uuid::Uuid::now_v7();
    let run_id = uuid::Uuid::now_v7();
    owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_start",
            "session_id": session_id, "branch_id": branch_id,
            "run_id": run_id, "input": "read README.txt",
        }),
    )
    .await;

    let probe = state_root.clone();
    wait_for("the approval", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;
    workload(&socket, &LocalRequest::Approve { run_id }).await;

    let probe = state_root.clone();
    wait_for("the Turn to land", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let turns = stored_turns(&socket, session_id, branch_id, 1).await;
    assert_eq!(turns.len(), 1, "one Turn must be stored");
    let (calls, results) = call_and_result_ids(&turns[0]);
    assert!(
        !calls.is_empty(),
        "this Turn is only interesting if it called a Tool"
    );
    for call in &calls {
        assert!(
            results.contains(call),
            "a stored Turn must not carry a Tool call with no result: call {call} of {calls:?}, \
             results {results:?}",
        );
    }
}

/// A Turn whose Tool was refused. The refusal is itself the Tool's result, so
/// the pair is complete -- which is the case a "no result" reading would most
/// easily get wrong.
#[tokio::test]
async fn a_refused_tool_still_leaves_a_result_in_the_stored_turn() {
    let state = tempfile::tempdir().expect("state");
    let workspace = workspace_with_a_readable_file();
    let state_root = state.path().to_path_buf();
    let (socket, _serving) = start(config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        spawn_provider().await,
    ))
    .await;

    let session_id = uuid::Uuid::now_v7();
    let branch_id = uuid::Uuid::now_v7();
    let run_id = uuid::Uuid::now_v7();
    owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_start",
            "session_id": session_id, "branch_id": branch_id,
            "run_id": run_id, "input": "read README.txt",
        }),
    )
    .await;

    let probe = state_root.clone();
    wait_for("the approval", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. })
        )
    })
    .await;
    workload(
        &socket,
        &LocalRequest::Deny {
            run_id,
            reason: Some("这个文件先别读".into()),
        },
    )
    .await;

    let probe = state_root.clone();
    wait_for("the Turn to land", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;

    let turns = stored_turns(&socket, session_id, branch_id, 1).await;
    assert_eq!(turns.len(), 1, "a refused Tool still completes the Turn");
    let (calls, results) = call_and_result_ids(&turns[0]);
    assert!(
        !calls.is_empty(),
        "the refused call must still be in the record"
    );
    for call in &calls {
        assert!(
            results.contains(call),
            "a refusal is the Tool's result and must be stored with it: call {call}, \
             results {results:?}",
        );
    }
}
