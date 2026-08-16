//! Long-running host and local IPC acceptance (ADR-0035 decision 7).
//!
//! The property under test is ownership: a Run belongs to the daemon and to the
//! local store, never to a client connection.

use agent_protocol::{RunBudget, RunStatus};
use agent_runtime_host::embedded::{
    RUNTIME_EVENT_CURSOR_SCHEMA_VERSION, RuntimeEventCursorErrorCode, RuntimeEventCursorRequest,
    RuntimeEventCursorState,
};
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRuntimeConfig, LocalToolConsent, WORKSPACE_READ_SCOPE,
    local_invocation_context,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};

async fn spawn_provider(turns: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        for body in turns {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
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

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
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
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    }
}

async fn start_daemon(config: LocalRuntimeConfig) -> PathBuf {
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind socket");
    let daemon = LocalRuntimeDaemon::new(config);
    tokio::spawn(daemon.serve(listener));
    socket
}

/// A whole client connection. Dropping it closes both halves, which is what
/// makes "the client disconnected" a real event rather than a half-open socket.
struct ClientConn {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    _writer: tokio::net::unix::OwnedWriteHalf,
}

/// Sends one request and reads its first response. The reader is kept intact:
/// rebuilding it would discard bytes the BufReader already pulled in, silently
/// dropping events that arrived in the same read.
async fn send(socket: &Path, request: &LocalRequest) -> (ClientConn, LocalResponse) {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request).expect("encode");
    line.push(b'\n');
    writer.write_all(&line).await.expect("write");
    writer.flush().await.expect("flush");
    let mut lines = BufReader::new(reader).lines();
    let response: LocalResponse = serde_json::from_str(
        &lines
            .next_line()
            .await
            .expect("read")
            .expect("a response line"),
    )
    .expect("decode");
    (
        ClientConn {
            lines,
            _writer: writer,
        },
        response,
    )
}

/// Reads responses until `Finished`, returning every event type seen.
async fn drain_stream(conn: ClientConn) -> (Vec<String>, String) {
    let mut lines = conn.lines;
    let mut event_types = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<LocalResponse>(&line).expect("decode") {
            LocalResponse::Event { event } => event_types.push(event.event_type),
            LocalResponse::Finished { status, .. } => return (event_types, status),
            LocalResponse::Error { message } => panic!("stream error: {message}"),
            _ => {}
        }
    }
    (event_types, "disconnected".into())
}

#[tokio::test]
async fn a_run_survives_the_client_that_submitted_it_disconnecting_immediately() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let endpoint = spawn_provider(vec![text_turn("daemon answer")]).await;
    let config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        endpoint,
    );
    let socket = start_daemon(config).await;

    let (stream, response) = send(
        &socket,
        &LocalRequest::Submit {
            input: "Summarize the workspace.".into(),
        },
    )
    .await;
    let LocalResponse::Accepted { run_id } = response else {
        panic!("expected acceptance, got {response:?}");
    };
    // The submitting client goes away before the Run can possibly be finished.
    drop(stream);

    // A brand new connection later must find the completed Run.
    let mut event_types = Vec::new();
    let mut status = String::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (stream, first) = send(
            &socket,
            &LocalRequest::Attach {
                run_id,
                after_sequence: 0,
            },
        )
        .await;
        let mut seen = match first {
            LocalResponse::Event { event } => vec![event.event_type],
            LocalResponse::Finished { status: done, .. } => {
                status = done;
                Vec::new()
            }
            other => panic!("unexpected first response: {other:?}"),
        };
        let (rest, done) = drain_stream(stream).await;
        seen.extend(rest);
        if done != "disconnected" {
            event_types = seen;
            status = done;
            break;
        }
    }

    assert_eq!(status, "succeeded", "events: {event_types:?}");
    assert!(
        event_types.contains(&"run.started".to_string())
            && event_types.contains(&"run.succeeded".to_string()),
        "a reconnecting client must see the whole Run: {event_types:?}"
    );
}

#[tokio::test]
async fn a_reconnecting_client_replays_the_durable_event_log_from_its_last_sequence() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let endpoint = spawn_provider(vec![text_turn("replay answer")]).await;
    let state_root = state.path().to_path_buf();
    let config = config(
        state_root.clone(),
        workspace.path().canonicalize().expect("canonical"),
        endpoint,
    );
    let socket = start_daemon(config).await;

    let (stream, response) = send(
        &socket,
        &LocalRequest::Submit {
            input: "Summarize the workspace.".into(),
        },
    )
    .await;
    let LocalResponse::Accepted { run_id } = response else {
        panic!("expected acceptance");
    };
    drop(stream);

    // Wait for the Run to finish, then attach twice with different cursors.
    let mut full = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (stream, first) = send(
            &socket,
            &LocalRequest::Attach {
                run_id,
                after_sequence: 0,
            },
        )
        .await;
        let mut seen = match first {
            LocalResponse::Event { event } => vec![event.event_type],
            _ => Vec::new(),
        };
        let (rest, done) = drain_stream(stream).await;
        seen.extend(rest);
        if done == "succeeded" {
            full = seen;
            break;
        }
    }
    assert!(!full.is_empty(), "the Run never produced durable events");

    // Attaching after the first event must return strictly fewer events, and
    // must not restart the stream from the beginning.
    let (stream, first) = send(
        &socket,
        &LocalRequest::Attach {
            run_id,
            after_sequence: 1,
        },
    )
    .await;
    let mut resumed = match first {
        LocalResponse::Event { event } => vec![event.event_type],
        _ => Vec::new(),
    };
    let (rest, _) = drain_stream(stream).await;
    resumed.extend(rest);

    assert_eq!(
        resumed.len() + 1,
        full.len(),
        "cursor replay dropped or duplicated events: full={full:?} resumed={resumed:?}"
    );
    assert_eq!(resumed, full[1..], "replay order changed");

    let (stream, response) = send(
        &socket,
        &LocalRequest::EventCursor {
            request: RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: local_invocation_context(),
                run_id,
                after_sequence: 0,
                limit: 1,
            },
        },
    )
    .await;
    drop(stream);
    let LocalResponse::EventCursor { page } = response else {
        panic!("expected typed event cursor page, got {response:?}");
    };
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.next_after_sequence, 1);
    assert_eq!(page.highest_committed_sequence, full.len() as u64);
    assert!(page.has_more);
    assert!(!page.history_gap);
    assert_eq!(
        page.state,
        RuntimeEventCursorState::Terminal {
            status: RunStatus::Succeeded
        }
    );

    let (stream, response) = send(
        &socket,
        &LocalRequest::EventCursor {
            request: RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: local_invocation_context(),
                run_id,
                after_sequence: full.len() as u64 + 1,
                limit: 1,
            },
        },
    )
    .await;
    drop(stream);
    let LocalResponse::EventCursorError { error } = response else {
        panic!("expected typed cursor-ahead error, got {response:?}");
    };
    assert_eq!(error.code, RuntimeEventCursorErrorCode::CursorAhead);

    let mut foreign = local_invocation_context();
    foreign.tenant_id = uuid::Uuid::now_v7();
    let (stream, response) = send(
        &socket,
        &LocalRequest::EventCursor {
            request: RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: foreign,
                run_id,
                after_sequence: 0,
                limit: 1,
            },
        },
    )
    .await;
    drop(stream);
    let LocalResponse::EventCursorError { error } = response else {
        panic!("expected typed identity error, got {response:?}");
    };
    assert_eq!(error.code, RuntimeEventCursorErrorCode::IdentityMismatch);
}

#[tokio::test]
async fn the_control_socket_is_owner_only() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let endpoint = spawn_provider(Vec::new()).await;
    let config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        endpoint,
    );
    let socket = start_daemon(config).await;

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&socket)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "whoever reaches this socket can spend the local provider credential"
    );
}

#[tokio::test]
async fn a_deeply_nested_state_root_still_yields_a_bindable_control_socket() {
    // Unix socket paths are capped by sockaddr_un (104 bytes on macOS, 108 on
    // Linux). A desktop state root such as
    // ~/Library/Application Support/<vendor>/<app>/<profile> passes that easily,
    // so deriving the socket path from the state root without a bound makes the
    // daemon unstartable for ordinary installs.
    let state = tempfile::tempdir().expect("state");
    let deep = state.path().join(
        "Library/Application Support/AgentRuntimePlatform/runtime-host/profiles/default/local",
    );
    std::fs::create_dir_all(&deep).expect("deep state root");

    let socket = default_socket_path(&deep);
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("a deeply nested state root must still bind a control socket");

    assert_eq!(
        default_socket_path(&deep),
        socket,
        "clients must derive the same socket path as the daemon"
    );
    LocalRuntimeDaemon::release(&socket, listener);
}

#[tokio::test]
async fn a_replacement_daemon_attaches_from_durable_state_without_an_in_memory_handle() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let endpoint = spawn_provider(vec![text_turn("durable replacement answer")]).await;
    let config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        endpoint,
    );
    let socket = default_socket_path(&config.state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind first");
    let first = LocalRuntimeDaemon::new(config.clone());
    let serving = tokio::spawn(first.serve(listener));

    let (client, response) = send(
        &socket,
        &LocalRequest::Submit {
            input: "Return one durable answer.".into(),
        },
    )
    .await;
    drop(client);
    let LocalResponse::Accepted { run_id } = response else {
        panic!("expected acceptance, got {response:?}");
    };
    loop {
        let (stream, first) = send(
            &socket,
            &LocalRequest::Attach {
                run_id,
                after_sequence: 0,
            },
        )
        .await;
        match first {
            LocalResponse::Finished { status, .. } if status == "succeeded" => {
                drop(stream);
                break;
            }
            LocalResponse::Event { .. } => {
                let (_, status) = drain_stream(stream).await;
                if status == "succeeded" {
                    break;
                }
            }
            other => panic!("unexpected attach response: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    serving.abort();
    let _ = serving.await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let replacement = loop {
        match LocalRuntimeDaemon::new_for_invocation(config.clone(), local_invocation_context()) {
            Ok(daemon) => break daemon,
            Err(error)
                if error
                    .to_string()
                    .contains("Workspace state root already has another Runtime owner") =>
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the predecessor daemon did not release its state-root lease"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => panic!("replacement daemon failed to start: {error}"),
        }
    };
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind replacement");
    assert_eq!(
        replacement.recover_unfinished().await.expect("reconcile"),
        0
    );
    let replacement_serving = tokio::spawn(replacement.serve(listener));

    let (stream, first) = send(
        &socket,
        &LocalRequest::Attach {
            run_id,
            after_sequence: 0,
        },
    )
    .await;
    let mut events = match first {
        LocalResponse::Event { event } => vec![event.event_type],
        other => panic!("replacement did not replay the durable log: {other:?}"),
    };
    let (rest, status) = drain_stream(stream).await;
    events.extend(rest);
    assert_eq!(status, "succeeded");
    assert!(events.iter().any(|event| event == "run.succeeded"));
    replacement_serving.abort();
}
