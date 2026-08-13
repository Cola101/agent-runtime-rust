use agent_protocol::{RunBudget, RuntimeExecutionPolicySnapshot, SubagentRole};
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRuntimeConfig, LocalRuntimeHost,
    LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};

fn subagent_tool_call_turn() -> String {
    let arguments = serde_json::json!({
        "role": "reviewer",
        "input": "Review until cancelled.",
        "max_tokens": 400,
        "max_cost_cents": 30,
        "max_duration_seconds": 60
    })
    .to_string();
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_cancel_child",
                    "type": "function",
                    "function": {"name": "agent.spawn", "arguments": arguments}
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn spawn_blocking_child_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("provider");
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent request");
        let mut request = vec![0_u8; 128 * 1024];
        let _ = parent.read(&mut request).await;
        let body = subagent_tool_call_turn();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        parent.write_all(response.as_bytes()).await.unwrap();
        parent.flush().await.unwrap();
        drop(parent);

        let (mut child, _) = listener.accept().await.expect("child request");
        let mut request = vec![0_u8; 128 * 1024];
        let read = child.read(&mut request).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("Review evidence only."));
        assert!(request.contains("Review until cancelled."));
        child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        child.flush().await.unwrap();
        let _ = started_tx.send(());

        // Keep a valid SSE response alive. The only correct way this finishes
        // is the Runtime cancelling the child's request and closing TCP.
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if child.write_all(b": still-running\n\n").await.is_err()
                || child.flush().await.is_err()
            {
                break;
            }
        }
        let _ = closed_tx.send(());
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        started_rx,
        closed_rx,
        handle,
    )
}

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

async fn write_sse(socket: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();
}

async fn spawn_recovery_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("provider");
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent request");
        let mut request = vec![0_u8; 128 * 1024];
        let _ = parent.read(&mut request).await;
        write_sse(&mut parent, &subagent_tool_call_turn()).await;

        let (mut first_child, _) = listener.accept().await.expect("first child request");
        let mut request = vec![0_u8; 128 * 1024];
        let _ = first_child.read(&mut request).await;
        first_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        first_child.flush().await.unwrap();
        let _ = started_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if first_child
                .write_all(b": interrupted-child\n\n")
                .await
                .is_err()
                || first_child.flush().await.is_err()
            {
                break;
            }
        }
        let _ = closed_tx.send(());

        let (mut restored_child, _) = listener.accept().await.expect("restored child request");
        let mut request = vec![0_u8; 128 * 1024];
        let read = restored_child.read(&mut request).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("Review until cancelled."));
        assert!(request.contains("Review evidence only."));
        write_sse(&mut restored_child, &text_turn("Recovered child result.")).await;

        let (mut restored_parent, _) = listener.accept().await.expect("restored parent request");
        let mut request = vec![0_u8; 128 * 1024];
        let read = restored_parent.read(&mut request).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("call_cancel_child"));
        assert!(request.contains("Recovered child result."));
        write_sse(&mut restored_parent, &text_turn("Parent resumed once.")).await;
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        started_rx,
        closed_rx,
        handle,
    )
}

async fn spawn_result_receipt_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("provider");
    let port = listener.local_addr().unwrap().port();
    let (child_started_tx, child_started_rx) = oneshot::channel();
    let (release_child_tx, release_child_rx) = oneshot::channel();
    let (parent_followup_tx, parent_followup_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent request");
        let mut request = vec![0_u8; 128 * 1024];
        let _ = parent.read(&mut request).await;
        write_sse(&mut parent, &subagent_tool_call_turn()).await;

        let (mut child, _) = listener.accept().await.expect("child request");
        let mut request = vec![0_u8; 128 * 1024];
        let _ = child.read(&mut request).await;
        let _ = child_started_tx.send(());
        let _ = release_child_rx.await;
        write_sse(&mut child, &text_turn("Receipt-backed child result.")).await;

        let (mut interrupted_parent, _) = listener.accept().await.expect("parent followup");
        let mut request = vec![0_u8; 128 * 1024];
        let read = interrupted_parent.read(&mut request).await.unwrap_or(0);
        assert!(String::from_utf8_lossy(&request[..read]).contains("Receipt-backed child result."));
        interrupted_parent
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        interrupted_parent.flush().await.unwrap();
        let _ = parent_followup_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if interrupted_parent
                .write_all(b": interrupted-parent\n\n")
                .await
                .is_err()
                || interrupted_parent.flush().await.is_err()
            {
                break;
            }
        }

        let (mut recovered_parent, _) = listener.accept().await.expect("recovered parent");
        let mut request = vec![0_u8; 128 * 1024];
        let read = recovered_parent.read(&mut request).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("call_cancel_child"));
        assert!(request.contains("Receipt-backed child result."));
        write_sse(
            &mut recovered_parent,
            &text_turn("Receipt recovery completed."),
        )
        .await;
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        child_started_rx,
        release_child_tx,
        parent_followup_rx,
        handle,
    )
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    let mut config = LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Delegate the review.".into(),
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
            endpoint,
            "local-test-model",
            "local-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::AllowOnce,
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

async fn request(socket: &std::path::Path, request: &LocalRequest) -> LocalResponse {
    let stream = UnixStream::connect(socket).await.expect("connect daemon");
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request).unwrap();
    line.push(b'\n');
    writer.write_all(&line).await.unwrap();
    writer.flush().await.unwrap();
    let mut lines = BufReader::new(reader).lines();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn cancelling_a_parent_while_its_child_model_is_streaming_closes_the_child() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, child_started, child_closed, provider) = spawn_blocking_child_provider().await;
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
    );
    let socket = default_socket_path(state.path());
    let listener = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("bind daemon");
    let daemon = LocalRuntimeDaemon::new(local_config);
    let serving = tokio::spawn(daemon.serve(listener));

    let LocalResponse::Accepted { run_id } = request(
        &socket,
        &LocalRequest::Submit {
            input: "Start the reviewer.".into(),
        },
    )
    .await
    else {
        panic!("submit was not accepted");
    };
    timeout(Duration::from_secs(3), child_started)
        .await
        .expect("child model request did not start")
        .expect("child start signal dropped");

    assert_eq!(
        request(&socket, &LocalRequest::Cancel { run_id }).await,
        LocalResponse::Accepted { run_id },
        "an active parent must accept cancellation"
    );
    timeout(Duration::from_secs(3), child_closed)
        .await
        .expect("child provider request survived parent cancellation")
        .expect("child close signal dropped");

    let cancelled = timeout(Duration::from_secs(3), async {
        loop {
            let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap();
            if events
                .iter()
                .any(|event| event.event_type == "run.cancelled")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if cancelled.is_err() {
        panic!(
            "parent never durably emitted run.cancelled; record={:?}, events={:?}",
            LocalRuntimeHost::read_run_record(state.path(), run_id).unwrap(),
            LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap()
        );
    }

    provider.await.expect("provider lifecycle");
    serving.abort();
}

#[tokio::test]
async fn restarting_the_host_resumes_the_same_child_without_spawning_again() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, child_started, first_child_closed, provider) = spawn_recovery_provider().await;
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
    );
    let parent_run_id = uuid::Uuid::now_v7();
    let first_config = local_config.clone();
    let first = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(parent_run_id, "Start the reviewer.").await
    });
    timeout(Duration::from_secs(3), child_started)
        .await
        .expect("child model request did not start")
        .expect("child start signal dropped");
    assert!(
        LocalRuntimeHost::checkpoint_path(state.path(), parent_run_id).is_file(),
        "parent spawn request was not durable before the child started"
    );

    first.abort();
    let _ = first.await;
    timeout(Duration::from_secs(3), first_child_closed)
        .await
        .expect("interrupted child connection stayed open")
        .expect("child close signal dropped");

    let mut replacement = LocalRuntimeHost::start(local_config).unwrap();
    let outcome = replacement
        .resume(parent_run_id, "Start the reviewer.", 2)
        .await
        .expect("parent and existing child recover");
    replacement.shutdown().await;
    provider.await.expect("provider recovery sequence");

    assert_eq!(outcome.output, "Parent resumed once.");
    let parent_events = LocalRuntimeHost::replay_events(state.path(), parent_run_id, 0).unwrap();
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "subagent.spawn.requested")
            .count(),
        1,
        "recovery asked the parent model to spawn the child again"
    );
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| event.event_type == "subagent.result.received")
            .count(),
        1
    );
    let run_directories = std::fs::read_dir(state.path().join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("checkpoint.json").is_file())
        .count();
    assert_eq!(
        run_directories, 2,
        "restart created another child identity instead of resuming the first"
    );
}

#[tokio::test]
async fn recovery_reuses_a_durable_child_result_without_calling_the_child_again() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, child_started, release_child, parent_followup, provider) =
        spawn_result_receipt_provider().await;
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
    );
    let parent_run_id = uuid::Uuid::now_v7();
    let first_config = local_config.clone();
    let first = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(parent_run_id, "Start the reviewer.").await
    });
    timeout(Duration::from_secs(3), child_started)
        .await
        .expect("child did not start")
        .expect("child start signal dropped");

    let parent_checkpoint = LocalRuntimeHost::checkpoint_path(state.path(), parent_run_id);
    let pending_checkpoint = std::fs::read(&parent_checkpoint).expect("pending parent checkpoint");
    let parent_event_log = state
        .path()
        .join("runs")
        .join(parent_run_id.to_string())
        .join("events.jsonl");
    let pending_events = std::fs::read(&parent_event_log).expect("pending parent events");
    release_child.send(()).expect("release child");
    timeout(Duration::from_secs(3), parent_followup)
        .await
        .expect("parent did not receive child result")
        .expect("parent followup signal dropped");

    let receipt_count = std::fs::read_dir(
        state
            .path()
            .join("runs")
            .join(parent_run_id.to_string())
            .join("subagents"),
    )
    .expect("durable subagent receipt directory")
    .filter_map(Result::ok)
    .count();
    assert_eq!(
        receipt_count, 1,
        "child result was not durable before parent continuation"
    );

    first.abort();
    let _ = first.await;
    // Recreate the exact crash window: the child result receipt is durable,
    // while the parent still has the pre-result pending-spawn Checkpoint/log.
    std::fs::write(&parent_checkpoint, pending_checkpoint).unwrap();
    std::fs::write(&parent_event_log, pending_events).unwrap();
    let parent_run_name = parent_run_id.to_string();
    let child_run_dir = std::fs::read_dir(state.path().join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name().and_then(|name| name.to_str()) != Some(parent_run_name.as_str())
        })
        .expect("child run directory");
    std::fs::write(child_run_dir.join("events.jsonl"), []).unwrap();

    let mut replacement = LocalRuntimeHost::start(local_config).unwrap();
    let outcome = replacement
        .resume(parent_run_id, "Start the reviewer.", 2)
        .await
        .expect("receipt resumes parent");
    replacement.shutdown().await;
    provider.await.expect("provider receipt recovery");

    assert_eq!(outcome.output, "Receipt recovery completed.");
    assert_eq!(
        std::fs::read_dir(state.path().join("runs"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("checkpoint.json").is_file())
            .count(),
        2,
        "receipt recovery created a duplicate child Run"
    );
}
