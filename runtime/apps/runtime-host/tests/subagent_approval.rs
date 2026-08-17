//! Nested approval routing for the standalone parent/child Agent loop.
//!
//! The provider, trusted workspace Tool, Unix IPC daemon and filesystem
//! Checkpoints are real. Only the model is a deterministic loopback peer.

use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, SubagentRole};
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunRecord, LocalRunState,
    LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use uuid::Uuid;

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

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn tool_call_turn(name: &str, call_id: &str, arguments: serde_json::Value) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn read_json_request(socket: &mut tokio::net::TcpStream) -> Option<serde_json::Value> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let (header_end, content_length) = loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("read provider request");
        if read == 0 && request.is_empty() {
            return None;
        }
        assert!(read > 0, "provider request closed with partial headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            let headers = std::str::from_utf8(&request[..header_end]).expect("HTTP headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                })
                .expect("JSON request content length");
            break (header_end, content_length);
        }
        assert!(
            request.len() < 512 * 1024,
            "provider request is unexpectedly large"
        );
    };
    while request.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.expect("read provider body");
        assert!(read > 0, "provider request closed before its body");
        request.extend_from_slice(&chunk[..read]);
    }
    Some(
        serde_json::from_slice(&request[header_end..header_end + content_length])
            .expect("provider request JSON"),
    )
}

async fn spawn_nested_approval_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider addr").port();
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            let Some(request) = read_json_request(&mut socket).await else {
                continue;
            };
            let request = request.to_string();
            // Select by durable transcript content instead of connection
            // ordinal. The post-approval child request may be abandoned by the
            // deliberately crashed daemon and retried by its replacement.
            let (body, expected, terminal) = if request.contains("subagent_run_id") {
                (
                    text_turn("Parent accepted the child review."),
                    vec![
                        "call_reviewer",
                        "Child verified the durable evidence.",
                        "subagent_run_id",
                    ],
                    true,
                )
            } else if request.contains("call_child_read") {
                (
                    text_turn("Child verified the durable evidence."),
                    vec![
                        "call_child_read",
                        "durable nested approval evidence",
                        "workspace.read_text",
                    ],
                    false,
                )
            } else if request.contains("Review evidence only.") {
                (
                    tool_call_turn(
                        "workspace.read_text",
                        "call_child_read",
                        serde_json::json!({"path": "EVIDENCE.txt"}),
                    ),
                    vec!["Review evidence only.", "workspace.read_text"],
                    false,
                )
            } else {
                (
                    tool_call_turn(
                        "agent.spawn",
                        "call_reviewer",
                        serde_json::json!({
                            "role": "reviewer",
                            "input": "Read the evidence file and review it.",
                            "max_tokens": 400,
                            "max_cost_cents": 30,
                            "max_duration_seconds": 20
                        }),
                    ),
                    vec!["agent.spawn", "reviewer"],
                    false,
                )
            };
            for needle in expected {
                assert!(
                    request.contains(needle),
                    "provider request did not contain {needle}: {request}"
                );
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            if terminal {
                break;
            }
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

#[tokio::test]
async fn nested_approval_provider_ignores_an_empty_abandoned_connection() {
    let (endpoint, provider) = spawn_nested_approval_provider().await;
    let authority = endpoint
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix("/v1/chat/completions"))
        .expect("loopback provider authority");

    let abandoned = TcpStream::connect(authority)
        .await
        .expect("connect abandoned request");
    drop(abandoned);

    let mut socket = TcpStream::connect(authority)
        .await
        .expect("connect valid request");
    let body = serde_json::json!({
        "messages": [{
            "content": "call_reviewer Child verified the durable evidence. subagent_run_id"
        }]
    })
    .to_string();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write valid request");
    let mut response = Vec::new();
    socket
        .read_to_end(&mut response)
        .await
        .expect("read valid response");
    assert!(
        String::from_utf8(response)
            .expect("provider response UTF-8")
            .contains("Parent accepted the child review.")
    );
    provider
        .await
        .expect("provider survived abandoned connection");
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    provider_endpoint: String,
    trusted_tool: PathBuf,
) -> LocalRuntimeConfig {
    let mut config = LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Delegate evidence review to the reviewer.".into(),
        delegated_scopes: BTreeSet::from([
            "agent:spawn".to_owned(),
            WORKSPACE_READ_SCOPE.to_owned(),
        ]),
        subagent_roles: vec![SubagentRole {
            name: "reviewer".into(),
            instructions: "Review evidence only.".into(),
            delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        }],
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            provider_endpoint,
            "local-test-model",
            "local-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: Some(trusted_tool),
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    };
    config
        .model_routing
        .health_policy
        .max_same_provider_attempts = 2;
    config
}

async fn request(socket: &Path, request: &LocalRequest) -> LocalResponse {
    let stream = UnixStream::connect(socket).await.expect("connect daemon");
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request).expect("encode request");
    line.push(b'\n');
    writer.write_all(&line).await.expect("write request");
    writer.flush().await.expect("flush request");
    let mut lines = BufReader::new(reader).lines();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).expect("decode response")
}

async fn start_daemon(config: LocalRuntimeConfig) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind daemon");
    let daemon = LocalRuntimeDaemon::new(config);
    daemon.recover_unfinished().await.expect("recover daemon");
    let serving = tokio::spawn(daemon.serve(listener));
    (socket, serving)
}

async fn wait_for_approval(state_root: &Path, run_id: Uuid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match LocalRuntimeHost::read_run_record(state_root, run_id) {
            Ok(Some(record)) if matches!(record.state, LocalRunState::AwaitingApproval { .. }) => {
                return;
            }
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. }) => {
                panic!(
                    "the child approval was collapsed into a parent terminal state: {:?}",
                    record.state
                );
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("timed out waiting for nested approval");
}

async fn wait_for_success(state_root: &Path, run_id: Uuid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if matches!(
            LocalRuntimeHost::read_run_record(state_root, run_id),
            Ok(Some(record))
                if record.state == LocalRunState::Finished { status: "succeeded".into() }
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let record = LocalRuntimeHost::read_run_record(state_root, run_id)
        .expect("read timed-out parent record");
    panic!("timed out waiting for parent success; final record: {record:?}");
}

async fn wait_for_consumed_approval_checkpoint(state_root: &Path, child_run_id: Uuid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let checkpoint_path = LocalRuntimeHost::checkpoint_path(state_root, child_run_id);
        if let Ok(checkpoint) = LocalRuntimeHost::load_checkpoint(&checkpoint_path) {
            let state: serde_json::Value =
                serde_json::from_slice(&checkpoint.state).expect("decode worker checkpoint state");
            let has_tool_result = LocalRuntimeHost::replay_events(state_root, child_run_id, 0)
                .expect("child events")
                .iter()
                .any(|event| event.event_type == "tool.result");
            if state["pending_approval"].is_null() && has_tool_result {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for the consumed approval checkpoint");
}

#[tokio::test]
async fn a_child_tool_approval_routes_through_the_parent_and_survives_a_daemon_restart() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "durable nested approval evidence",
    )
    .expect("evidence fixture");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let (provider_endpoint, provider) = spawn_nested_approval_provider().await;
    let local_config = config(
        state_root.clone(),
        workspace_root,
        provider_endpoint,
        trusted_tool,
    );
    let (socket, first_daemon) = start_daemon(local_config.clone()).await;
    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Review the evidence through the reviewer.".into(),
        },
    )
    .await
    else {
        panic!("parent Run was not accepted");
    };

    wait_for_approval(&state_root, run_id).await;
    let child_run_id = std::fs::read_dir(state_root.join("runs"))
        .expect("run dirs")
        .filter_map(Result::ok)
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .find(|candidate| *candidate != run_id)
        .expect("child run directory");
    let child_before =
        LocalRuntimeHost::replay_events(&state_root, child_run_id, 0).expect("child events");
    assert!(
        child_before
            .iter()
            .any(|event| event.event_type == "approval.required")
    );
    assert!(
        !child_before
            .iter()
            .any(|event| event.event_type == "tool.execution.started"),
        "the child Tool executed before operator approval"
    );

    first_daemon.abort();
    let _ = first_daemon.await;
    let approval_state_root = state_root.clone();
    let approval_config = local_config.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("approval daemon runtime");
        runtime.block_on(async move {
            let (replacement_socket, replacement) = start_daemon(approval_config).await;
            assert_eq!(
                request(&replacement_socket, &LocalRequest::Approve { run_id }).await,
                LocalResponse::Accepted { run_id }
            );
            let acknowledged = LocalRuntimeHost::read_run_record(&approval_state_root, run_id)
                .expect("record readable after approval acknowledgement")
                .expect("record present after approval acknowledgement");
            assert!(
                matches!(acknowledged.state, LocalRunState::ApprovalDecided { .. }),
                "an acknowledged approval decision must be recoverable, not process-local"
            );
            assert_eq!(
                request(&replacement_socket, &LocalRequest::Approve { run_id }).await,
                LocalResponse::Accepted { run_id },
                "the same decision must be idempotent"
            );
            assert!(
                matches!(
                    request(&replacement_socket, &LocalRequest::Deny { run_id }).await,
                    LocalResponse::Error { .. }
                ),
                "a conflicting second decision must fail closed"
            );
            wait_for_consumed_approval_checkpoint(&approval_state_root, child_run_id).await;
            replacement.abort();
        });
        // Dropping this runtime also drops the resumed Run after its child
        // Checkpoint consumed the decision but before the parent can finish.
    })
    .join()
    .expect("approval daemon thread");

    let (_recovered_socket, recovered) = start_daemon(local_config).await;
    wait_for_success(&state_root, run_id).await;
    provider.await.expect("all model turns observed");

    let child_after =
        LocalRuntimeHost::replay_events(&state_root, child_run_id, 0).expect("child events");
    assert_eq!(
        child_after
            .iter()
            .filter(|event| event.event_type == "tool.execution.started")
            .count(),
        1
    );
    let parent_events =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("parent events");
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "subagent.result.received")
            .count(),
        1
    );
    recovered.abort();
}

#[tokio::test]
async fn a_child_approval_bound_to_another_run_is_rejected_without_terminalizing_the_parent() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "durable nested approval evidence",
    )
    .expect("evidence fixture");
    let state_root = state.path().to_path_buf();
    let (provider_endpoint, provider) = spawn_nested_approval_provider().await;
    let local_config = config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        trusted_tool,
    );
    let (socket, first_daemon) = start_daemon(local_config.clone()).await;
    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Review the evidence through the reviewer.".into(),
        },
    )
    .await
    else {
        panic!("parent Run was not accepted");
    };
    wait_for_approval(&state_root, run_id).await;
    let child_run_id = std::fs::read_dir(state_root.join("runs"))
        .expect("run dirs")
        .filter_map(Result::ok)
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .find(|candidate| *candidate != run_id)
        .expect("child run directory");

    first_daemon.abort();
    let _ = first_daemon.await;
    let record = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("root record readable")
        .expect("root record present");
    let LocalRunState::AwaitingApproval {
        approval_id,
        binding_digest,
        ..
    } = record.state
    else {
        panic!("root Run was not awaiting the child approval");
    };
    LocalRuntimeHost::write_run_record(
        &state_root,
        &LocalRunRecord {
            state: LocalRunState::AwaitingApproval {
                approval_id,
                binding_digest,
                target_run_id: Some(Uuid::now_v7()),
            },
            ..record
        },
    )
    .expect("write misdirected approval fixture");

    let (replacement_socket, replacement) = start_daemon(local_config).await;
    assert!(
        matches!(
            request(&replacement_socket, &LocalRequest::Approve { run_id }).await,
            LocalResponse::Error { .. }
        ),
        "a decision that disagrees with the Checkpoint must be rejected before acceptance"
    );
    let after = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("root record remains readable")
        .expect("root record remains present");
    assert!(
        matches!(after.state, LocalRunState::AwaitingApproval { .. }),
        "a rejected control command must not manufacture a terminal Run state"
    );
    let parent_events =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("parent events");
    assert!(
        !parent_events.iter().any(|event| matches!(
            event.event_type.as_str(),
            "run.failed" | "run.cancelled" | "run.timed_out" | "run.indeterminate"
        )),
        "a rejected control command must not manufacture a Kernel terminal event"
    );
    assert!(
        std::fs::read_dir(state_root.join("control-receipts"))
            .map(|entries| entries.count() == 0)
            .unwrap_or(true),
        "a rejected control command must not leave an accepted receipt"
    );
    let child_events =
        LocalRuntimeHost::replay_events(&state_root, child_run_id, 0).expect("child events");
    assert!(
        !child_events
            .iter()
            .any(|event| event.event_type == "tool.execution.started"),
        "a decision bound to another Run must never execute this child's Tool"
    );
    provider.abort();
    replacement.abort();
}
