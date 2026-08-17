//! Standalone multi-Provider acceptance.
//!
//! The test crosses three real loopback HTTP/SSE protocol boundaries. It is
//! intentionally a Host test, not an adapter test: a green adapter suite does
//! not prove that the independent Runtime can route or recover a Run.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    ModelErrorKind, RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, SubagentRole,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig,
    LocalProviderHealthPolicy, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug)]
struct CapturedRequest {
    head: String,
    body: serde_json::Value,
}

fn committed_route_records(path: &Path) -> Vec<serde_json::Value> {
    let body = std::fs::read(path).expect("route WAL");
    let committed_length = body
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    body[..committed_length]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("route WAL record"))
        .collect()
}

fn current_route_journal(path: &Path) -> serde_json::Value {
    committed_route_records(path)
        .last()
        .and_then(|record| record.get("journal"))
        .cloned()
        .expect("current route journal snapshot")
}

async fn read_request(socket: &mut TcpStream) -> CapturedRequest {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "request ended before its headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            let head = std::str::from_utf8(&request[..header_end]).expect("HTTP head");
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                })
                .expect("content length header");
            break (header_end, content_length);
        }
    };
    while request.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "request ended before its body");
        request.extend_from_slice(&chunk[..read]);
    }
    CapturedRequest {
        head: String::from_utf8(request[..header_end].to_vec()).expect("HTTP head UTF-8"),
        body: serde_json::from_slice(&request[header_end..header_end + content_length])
            .expect("JSON request"),
    }
}

async fn spawn_response(
    status: u16,
    body: &'static str,
) -> (
    String,
    tokio::sync::oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let request = read_request(&mut socket).await;
        let reason = if status == 200 { "OK" } else { "Unavailable" };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        captured_tx.send(request).ok();
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    (format!("http://{address}"), captured_rx, server)
}

async fn spawn_repeated_responses(
    bodies: Vec<&'static str>,
) -> (String, tokio::task::JoinHandle<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            requests.push(read_request(&mut socket).await);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (format!("http://{address}/v1/responses"), server)
}

async fn spawn_must_not_run() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        if let Ok(Ok((_socket, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept()).await
        {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    (format!("http://{address}"), calls, server)
}

async fn spawn_retry_then_compatible_success()
-> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first provider attempt");
        let _ = read_request(&mut first).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let error = r#"{"error":{"message":"transient first attempt"}}"#;
        let unavailable = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}",
            error.len()
        );
        first.write_all(unavailable.as_bytes()).await.unwrap();
        first.flush().await.unwrap();

        let (mut second, _) = listener.accept().await.expect("same-provider retry");
        let _ = read_request(&mut second).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"same provider recovered\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let success = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        second.write_all(success.as_bytes()).await.unwrap();
        second.flush().await.unwrap();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        calls,
        server,
    )
}

async fn spawn_repeated_compatible_responses(
    statuses: Vec<u16>,
    success_text: &'static str,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        for status in statuses {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await
            else {
                return;
            };
            let _ = read_request(&mut socket).await;
            observed.fetch_add(1, Ordering::SeqCst);
            let response = if status == 200 {
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{success_text}\"}},\"finish_reason\":null}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                     data: [DONE]\n\n"
                );
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let body = r#"{"error":{"message":"provider unavailable"}}"#;
                format!(
                    "HTTP/1.1 {status} Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (
        format!("http://{address}/v1/chat/completions"),
        calls,
        server,
    )
}

async fn spawn_retry_after_then_watch_for_replay()
-> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("rate-limited request");
        let _ = read_request(&mut socket).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let body = r#"{"error":{"message":"slow down"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        if let Ok(Ok((_unexpected, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
        {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    (
        format!("http://{address}/v1/chat/completions"),
        calls,
        server,
    )
}

async fn spawn_half_open_compatible_provider() -> (
    String,
    Arc<AtomicUsize>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let (probe_seen_tx, probe_seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("initial provider request");
        let _ = read_request(&mut first).await;
        observed.fetch_add(1, Ordering::SeqCst);
        let body = r#"{"error":{"message":"temporarily unavailable"}}"#;
        let response = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        first.write_all(response.as_bytes()).await.unwrap();
        first.flush().await.unwrap();

        let (mut probe, _) = listener.accept().await.expect("half-open probe");
        let _ = read_request(&mut probe).await;
        observed.fetch_add(1, Ordering::SeqCst);
        probe_seen_tx.send(()).ok();
        let _ = release_rx.await;
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"probe recovered\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        probe.write_all(response.as_bytes()).await.unwrap();
        probe.flush().await.unwrap();
        if let Ok(Ok((_unexpected, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept()).await
        {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    (
        format!("http://{address}/v1/chat/completions"),
        calls,
        probe_seen_rx,
        release_tx,
        server,
    )
}

async fn spawn_recoverable_anthropic() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first fallback request");
        let _ = read_request(&mut first).await;
        observed.fetch_add(1, Ordering::SeqCst);
        first_seen_tx.send(()).ok();
        let _ = release_rx.await;
        drop(first);

        let (mut second, _) = listener.accept().await.expect("recovered fallback request");
        let request = read_request(&mut second).await;
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(request.head.starts_with("POST /v1/messages "));
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"recovered route\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        second.write_all(response.as_bytes()).await.unwrap();
        second.flush().await.unwrap();
    });
    (
        format!("http://{address}/v1/messages"),
        first_seen_rx,
        release_tx,
        calls,
        server,
    )
}

async fn spawn_partial_then_stall(
    stall: std::time::Duration,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("partial request");
        let _ = read_request(&mut socket).await;
        let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"committed\"},\"finish_reason\":null}]}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{partial}"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        tokio::time::sleep(stall).await;
    });
    (format!("http://{address}/v1/chat/completions"), server)
}

async fn spawn_controlled_compatible_success() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("controlled request");
        let _ = read_request(&mut socket).await;
        seen_tx.send(()).ok();
        let _ = release_rx.await;
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"staged answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        seen_rx,
        release_tx,
        server,
    )
}

fn candidate(
    id: &str,
    protocol: ProviderProtocol,
    endpoint: String,
    latency_ms: u64,
) -> LocalProviderConfig {
    LocalProviderConfig {
        id: id.into(),
        protocol,
        endpoint,
        model: format!("{id}-model"),
        api_key: format!("{id}-secret"),
        region: "local".into(),
        accepted_data_classes: BTreeSet::from([DataClass::Internal]),
        capabilities: BTreeSet::from([Capability::Text]),
        healthy: true,
        latency_ms,
        cost_per_million_tokens_micros: 0,
        response_timeout_ms: 1_000,
        stream_idle_timeout_ms: 100,
    }
}

#[test]
fn local_provider_debug_never_exposes_the_api_key() {
    let provider = candidate(
        "redacted",
        ProviderProtocol::OpenAiCompatible,
        "http://127.0.0.1:9/v1/chat/completions".into(),
        1,
    );

    let rendered = format!("{provider:?}");

    assert!(rendered.contains("redacted"));
    assert!(!rendered.contains("redacted-secret"));
    assert!(rendered.contains("[REDACTED]"));
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    candidates: Vec<LocalProviderConfig>,
) -> LocalRuntimeConfig {
    let mut runtime_policy = RuntimeExecutionPolicySnapshot::default();
    runtime_policy.model_failover.max_provider_attempts = 3;
    runtime_policy.model_failover.fallback_on = BTreeSet::from([ModelErrorKind::Unavailable]);
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Answer from evidence.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig {
            candidates,
            allowed_regions: BTreeSet::from(["local".into()]),
            data_class: DataClass::Internal,
            max_cost_per_million_tokens_micros: 10,
            health_policy: LocalProviderHealthPolicy::default(),
        },
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy,
    }
}

#[tokio::test]
async fn standalone_session_replays_openai_reasoning_state_only_to_its_origin_route() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let first = concat!(
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_session\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checked session context.\"}],\"encrypted_content\":\"enc-session\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"first answer\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n"
    );
    let second = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"continued answer\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
    );
    let (endpoint, provider) = spawn_repeated_responses(vec![first, second, second, second]).await;
    let mut host = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![candidate(
            "openai-primary",
            ProviderProtocol::OpenAiResponses,
            endpoint,
            1,
        )],
    ))
    .expect("Host");

    let started = host
        .start_session("Start the reasoning session.")
        .await
        .unwrap();
    assert_eq!(started.run.output, "first answer");
    assert!(
        started
            .run
            .event_types
            .iter()
            .any(|event| event == "model.reasoning")
    );
    let continued = host
        .continue_session(
            started.head.session_id,
            started.head.branch_id,
            started.head.generation,
            "Continue the same session.",
        )
        .await
        .unwrap();
    assert_eq!(continued.run.output, "continued answer");

    let fork = host
        .fork_session(
            continued.head.session_id,
            continued.head.branch_id,
            continued.head.generation,
            1,
        )
        .unwrap();
    let forked = host
        .continue_session(
            fork.session_id,
            fork.branch_id,
            fork.generation,
            "Continue from a fork of the reasoning turn.",
        )
        .await
        .unwrap();
    assert_eq!(forked.run.output, "continued answer");

    let rollback = host
        .rollback_session(
            continued.head.session_id,
            continued.head.branch_id,
            continued.head.generation,
            1,
        )
        .unwrap();
    let rolled = host
        .continue_session(
            rollback.session_id,
            rollback.branch_id,
            rollback.generation,
            "Continue from a rollback to the reasoning turn.",
        )
        .await
        .unwrap();
    assert_eq!(rolled.run.output, "continued answer");

    let requests = provider.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(!requests[0].body.to_string().contains("enc-session"));
    for request in &requests[1..] {
        let replayed = request.body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "reasoning")
            .expect("continue, fork, and rollback must replay the durable reasoning item");
        assert_eq!(replayed["id"], "rs_session");
        assert_eq!(replayed["encrypted_content"], "enc-session");
    }
}

#[tokio::test]
async fn standalone_host_returns_a_typed_refusal_instead_of_a_blank_success() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let body = concat!(
        "event: response.refusal.done\ndata: {\"type\":\"response.refusal.done\",\"refusal\":\"I cannot perform that request.\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n"
    );
    let (base, captured, server) = spawn_response(200, body).await;
    let mut host = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![candidate(
            "openai-primary",
            ProviderProtocol::OpenAiResponses,
            format!("{base}/v1/responses"),
            1,
        )],
    ))
    .unwrap();

    let outcome = host.execute("Ask for a typed refusal.").await.unwrap();

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "I cannot perform that request.");
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "model.refusal")
    );
    captured.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn zero_output_failure_retries_the_same_provider_before_crossing_candidates() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_calls, primary_server) =
        spawn_retry_then_compatible_success().await;
    let (fallback_base, fallback_calls, fallback_server) = spawn_must_not_run().await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "primary",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "fallback",
                ProviderProtocol::OpenAiCompatible,
                format!("{fallback_base}/v1/chat/completions"),
                2,
            ),
        ],
    );
    cfg.model_routing.health_policy.max_same_provider_attempts = 2;
    cfg.model_routing.health_policy.initial_retry_backoff_ms = 30;
    cfg.model_routing.health_policy.max_retry_backoff_ms = 30;
    cfg.model_routing
        .health_policy
        .consecutive_failure_threshold = 3;
    let started = std::time::Instant::now();
    let mut host = LocalRuntimeHost::start(cfg).expect("Host");

    let outcome = host
        .execute("retry safely before fallback")
        .await
        .expect("same Provider retry succeeds");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "same provider recovered");
    assert!(started.elapsed() >= std::time::Duration::from_millis(30));
    assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "model.provider.retry_scheduled")
    );
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn an_exhausted_single_provider_budget_is_a_terminal_run_without_replacement() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (endpoint, calls, server) = spawn_repeated_compatible_responses(vec![503], "unused").await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![candidate(
            "only-provider",
            ProviderProtocol::OpenAiCompatible,
            endpoint,
            1,
        )],
    );
    cfg.model_routing.health_policy.max_same_provider_attempts = 1;
    let mut host = LocalRuntimeHost::start(cfg).expect("Host");

    let outcome = host
        .execute("fail after the frozen Provider budget is exhausted")
        .await
        .expect("Provider exhaustion is a Run outcome, not a Host failure");

    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.failed")
            .count(),
        1
    );
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .expect("terminal event log");
    assert_eq!(
        events.last().map(|event| event.event_type.as_str()),
        Some("run.failed")
    );
    server.await.expect("Provider fixture");
}

#[tokio::test]
async fn replacement_host_skips_a_provider_in_persisted_cooldown() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_calls, primary_server) =
        spawn_repeated_compatible_responses(vec![503, 503], "unused").await;
    let (fallback_endpoint, fallback_calls, fallback_server) =
        spawn_repeated_compatible_responses(vec![200, 200], "fallback answer").await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "cooling-primary",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "healthy-fallback",
                ProviderProtocol::OpenAiCompatible,
                fallback_endpoint,
                2,
            ),
        ],
    );
    cfg.model_routing.health_policy.max_same_provider_attempts = 1;
    cfg.model_routing
        .health_policy
        .consecutive_failure_threshold = 1;
    cfg.model_routing.health_policy.cooldown_ms = 2_000;

    let mut first = LocalRuntimeHost::start(cfg.clone()).expect("first Host");
    let first_outcome = first.execute("first Run opens the circuit").await.unwrap();
    assert_eq!(first_outcome.output, "fallback answer");
    drop(first);

    let mut replacement = LocalRuntimeHost::start(cfg).expect("replacement Host");
    let second_outcome = replacement
        .execute("second Run observes durable cooldown")
        .await
        .unwrap();
    assert_eq!(second_outcome.output, "fallback answer");
    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);

    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn retry_after_opens_durable_cooldown_before_the_failure_threshold() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_calls, primary_server) =
        spawn_retry_after_then_watch_for_replay().await;
    let (fallback_endpoint, fallback_calls, fallback_server) =
        spawn_repeated_compatible_responses(vec![200, 200], "fallback answer").await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "rate-limited-primary",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "healthy-fallback",
                ProviderProtocol::OpenAiCompatible,
                fallback_endpoint,
                2,
            ),
        ],
    );
    cfg.runtime_policy
        .model_failover
        .fallback_on
        .insert(ModelErrorKind::RateLimited);
    cfg.model_routing.health_policy.max_same_provider_attempts = 2;
    cfg.model_routing
        .health_policy
        .consecutive_failure_threshold = 8;
    cfg.model_routing.health_policy.max_retry_after_ms = 5_000;

    let mut first = LocalRuntimeHost::start(cfg.clone()).expect("first Host");
    assert_eq!(
        first.execute("respect Retry-After").await.unwrap().output,
        "fallback answer"
    );
    drop(first);

    let health_body = std::fs::read_to_string(state.path().join("model-provider-health.json"))
        .expect("durable Provider health");
    let health: serde_json::Value = serde_json::from_str(&health_body).expect("health JSON");
    let entry = health["entries"]
        .as_object()
        .and_then(|entries| entries.values().next())
        .expect("rate-limit health entry");
    assert_eq!(entry["consecutive_failures"], 1);
    assert!(
        entry["cooldown_until_unix_ms"].as_i64().unwrap() > chrono::Utc::now().timestamp_millis()
    );
    assert!(!health_body.contains("slow down"));
    assert!(!health_body.contains("rate-limited-primary-secret"));

    let mut replacement = LocalRuntimeHost::start(cfg).expect("replacement Host");
    assert_eq!(
        replacement
            .execute("new Run skips Retry-After cooldown")
            .await
            .unwrap()
            .output,
        "fallback answer"
    );
    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn expired_cooldown_allows_only_one_concurrent_half_open_probe() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_calls, probe_seen, release_probe, primary_server) =
        spawn_half_open_compatible_provider().await;
    let (fallback_endpoint, fallback_calls, fallback_server) =
        spawn_repeated_compatible_responses(vec![200, 200], "fallback answer").await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "recovering-primary",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "healthy-fallback",
                ProviderProtocol::OpenAiCompatible,
                fallback_endpoint,
                2,
            ),
        ],
    );
    cfg.model_routing.health_policy.max_same_provider_attempts = 1;
    cfg.model_routing
        .health_policy
        .consecutive_failure_threshold = 1;
    cfg.model_routing.health_policy.cooldown_ms = 50;
    cfg.model_routing.health_policy.half_open_probe_lease_ms = 2_000;

    let mut opener = LocalRuntimeHost::start(cfg.clone()).expect("opening Host");
    assert_eq!(
        opener.execute("open circuit").await.unwrap().output,
        "fallback answer"
    );
    drop(opener);
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;

    let mut first = LocalRuntimeHost::start(cfg.clone()).expect("first probing Host");
    let mut second = LocalRuntimeHost::start(cfg).expect("second probing Host");
    let first_run = tokio::spawn(async move { first.execute("concurrent Run A").await });
    let second_run = tokio::spawn(async move { second.execute("concurrent Run B").await });
    probe_seen
        .await
        .expect("one half-open probe reached Provider");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fallback_calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the non-probe Run used fallback while the probe lease was active");
    release_probe.send(()).ok();

    let first = first_run.await.unwrap().unwrap();
    let second = second_run.await.unwrap().unwrap();
    let outputs = BTreeSet::from([first.output, second.output]);
    assert_eq!(
        outputs,
        BTreeSet::from(["fallback answer".into(), "probe recovered".into()])
    );
    assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn authentication_failures_neither_fallback_nor_contaminate_provider_health() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_calls, primary_server) =
        spawn_repeated_compatible_responses(vec![401, 401], "unused").await;
    let (fallback_base, fallback_calls, fallback_server) = spawn_must_not_run().await;
    let cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "bad-credential",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "must-not-receive-auth-failover",
                ProviderProtocol::OpenAiCompatible,
                format!("{fallback_base}/v1/chat/completions"),
                2,
            ),
        ],
    );

    let mut first = LocalRuntimeHost::start(cfg.clone()).expect("first Host");
    let first_outcome = first.execute("authentication failure A").await.unwrap();
    assert_eq!(first_outcome.status, RunStatus::Failed);
    drop(first);

    let mut replacement = LocalRuntimeHost::start(cfg).expect("replacement Host");
    let second_outcome = replacement
        .execute("authentication failure B")
        .await
        .unwrap();
    assert_eq!(second_outcome.status, RunStatus::Failed);
    assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    assert!(!state.path().join("model-provider-health.json").exists());
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn standalone_host_freezes_and_crosses_three_protocol_candidates_before_output() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let unavailable = r#"{"error":{"message":"temporarily unavailable"}}"#;
    let (responses_base, responses_request, responses_server) =
        spawn_response(503, unavailable).await;
    let (anthropic_base, anthropic_request, anthropic_server) =
        spawn_response(503, unavailable).await;
    let compatible_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"routed answer\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let (compatible_base, compatible_request, compatible_server) =
        spawn_response(200, compatible_body).await;
    let (must_not_run_base, must_not_run_calls, must_not_run_server) = spawn_must_not_run().await;

    let candidates = vec![
        candidate(
            "responses",
            ProviderProtocol::OpenAiResponses,
            format!("{responses_base}/v1/responses"),
            1,
        ),
        candidate(
            "anthropic",
            ProviderProtocol::AnthropicMessages,
            format!("{anthropic_base}/v1/messages"),
            2,
        ),
        candidate(
            "compatible",
            ProviderProtocol::OpenAiCompatible,
            format!("{compatible_base}/v1/chat/completions"),
            3,
        ),
        candidate(
            "outside-frozen-budget",
            ProviderProtocol::OpenAiCompatible,
            format!("{must_not_run_base}/v1/chat/completions"),
            4,
        ),
    ];
    let mut host = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        candidates,
    ))
    .expect("start Host");

    let outcome = host
        .execute("route this request")
        .await
        .expect("Run succeeds");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "routed answer");
    assert_eq!(
        outcome.event_types,
        vec![
            "run.started",
            "model.provider.failed",
            "model.provider.failed",
            "model.provider.selected",
            "model.output.delta",
            "model.usage",
            "run.succeeded",
        ]
    );
    let responses_request = responses_request.await.expect("Responses request");
    let anthropic_request = anthropic_request.await.expect("Anthropic request");
    let compatible_request = compatible_request.await.expect("compatible request");
    assert!(responses_request.head.starts_with("POST /v1/responses "));
    assert_eq!(responses_request.body["model"], "responses-model");
    assert!(anthropic_request.head.starts_with("POST /v1/messages "));
    assert_eq!(anthropic_request.body["model"], "anthropic-model");
    assert!(
        compatible_request
            .head
            .starts_with("POST /v1/chat/completions ")
    );
    assert_eq!(compatible_request.body["model"], "compatible-model");
    responses_server.await.unwrap();
    anthropic_server.await.unwrap();
    compatible_server.await.unwrap();
    must_not_run_server.await.unwrap();
    assert_eq!(must_not_run_calls.load(Ordering::SeqCst), 0);

    let route_dir = state
        .path()
        .join("runs")
        .join(outcome.run_id.to_string())
        .join("model-routes");
    let journals = std::fs::read_dir(route_dir)
        .expect("route journal directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("route journal entries");
    assert_eq!(journals.len(), 1);
    let journal = current_route_journal(&journals[0].path());
    assert_eq!(
        journal["candidate_ids"],
        serde_json::json!(["responses", "anthropic", "compatible"])
    );
    assert_eq!(journal["failed_attempts"].as_array().unwrap().len(), 2);
    assert_eq!(journal["selected_provider_id"], "compatible");
    assert_eq!(journal["completed"], true);
}

#[tokio::test]
async fn replacement_host_resumes_the_checkpointed_candidate_cursor_without_replaying_primary() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let unavailable = r#"{"error":{"message":"temporarily unavailable"}}"#;
    let (primary_base, primary_request, primary_server) = spawn_response(503, unavailable).await;
    let (fallback_endpoint, first_seen, release, fallback_calls, fallback_server) =
        spawn_recoverable_anthropic().await;
    let candidates = vec![
        candidate(
            "primary",
            ProviderProtocol::OpenAiResponses,
            format!("{primary_base}/v1/responses"),
            1,
        ),
        candidate(
            "fallback",
            ProviderProtocol::AnthropicMessages,
            fallback_endpoint,
            2,
        ),
    ];
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        candidates,
    );
    cfg.model_routing.health_policy.max_same_provider_attempts = 2;
    let run_id = uuid::Uuid::now_v7();
    let mut first_host = LocalRuntimeHost::start(cfg.clone()).expect("first Host");
    let mut execution = Box::pin(first_host.execute_as(run_id, "recover this route"));
    tokio::select! {
        _ = first_seen => {}
        result = &mut execution => panic!("first Host unexpectedly completed: {result:?}"),
    }
    drop(execution);
    drop(first_host);
    release.send(()).ok();

    let mut replacement = LocalRuntimeHost::start(cfg).expect("replacement Host");
    let outcome = replacement
        .resume(run_id, "recover this route", 2)
        .await
        .expect("resume Run");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "recovered route");
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);
    let primary = primary_request.await.expect("primary request");
    assert!(primary.head.starts_with("POST /v1/responses "));
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn partial_stream_failure_is_preserved_and_never_crosses_to_a_fallback_provider() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (primary_endpoint, primary_server) =
        spawn_partial_then_stall(std::time::Duration::from_millis(300)).await;
    let (fallback_base, fallback_calls, fallback_server) = spawn_must_not_run().await;
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            candidate(
                "primary",
                ProviderProtocol::OpenAiCompatible,
                primary_endpoint,
                1,
            ),
            candidate(
                "must-not-run",
                ProviderProtocol::AnthropicMessages,
                format!("{fallback_base}/v1/messages"),
                2,
            ),
        ],
    );
    cfg.runtime_policy.model_failover.fallback_on = BTreeSet::from([ModelErrorKind::Timeout]);
    let mut host = LocalRuntimeHost::start(cfg).expect("Host");

    let outcome = host
        .execute("do not replay partial output")
        .await
        .expect("terminal Run");

    assert_eq!(outcome.status, RunStatus::TimedOut);
    assert_eq!(outcome.output, "committed");
    assert_eq!(
        outcome.event_types,
        vec![
            "run.started",
            "model.provider.failed",
            "model.output.delta",
            "run.timed_out",
        ]
    );
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replacement_host_applies_a_staged_terminal_response_without_replaying_the_provider() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let (endpoint, request_seen, release, server) = spawn_controlled_compatible_success().await;
    let cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![candidate(
            "primary",
            ProviderProtocol::OpenAiCompatible,
            endpoint,
            1,
        )],
    );
    let run_id = uuid::Uuid::now_v7();
    let mut first_host = LocalRuntimeHost::start(cfg.clone()).expect("first Host");
    let mut execution = Box::pin(first_host.execute_as(run_id, "stage this response"));
    tokio::select! {
        _ = request_seen => {}
        result = &mut execution => panic!("Run completed before the response gate: {result:?}"),
    }
    let run_dir = state.path().join("runs").join(run_id.to_string());
    let checkpoint_before_response =
        std::fs::read(run_dir.join("checkpoint.json")).expect("pre-response checkpoint");
    let events_before_response =
        std::fs::read(run_dir.join("events.jsonl")).expect("pre-response event log");
    release.send(()).ok();
    let completed = execution.await.expect("fixture Run completes");
    assert_eq!(completed.status, RunStatus::Succeeded);
    drop(first_host);
    server.await.unwrap();

    let route_dir = run_dir.join("model-routes");
    let journal_path = std::fs::read_dir(&route_dir)
        .expect("route dir")
        .next()
        .expect("journal entry")
        .expect("journal")
        .path();
    let records = committed_route_records(&journal_path);
    let staged_record_index = records
        .iter()
        .position(|record| {
            record["journal"]["completed"] == false
                && record["journal"]["selection_reported"] == false
                && record["journal"]["staged_events"]
                    .as_array()
                    .is_some_and(|events| !events.is_empty())
        })
        .expect("committed staged-response boundary");
    let mut committed_prefix = Vec::new();
    for record in &records[..=staged_record_index] {
        committed_prefix.extend(serde_json::to_vec(record).expect("route WAL record"));
        committed_prefix.push(b'\n');
    }
    std::fs::write(&journal_path, committed_prefix).expect("restore staged WAL prefix");
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .unwrap()
        .write_all(b"{\"record_version\":1")
        .unwrap();
    std::fs::write(run_dir.join("checkpoint.json"), checkpoint_before_response).unwrap();
    std::fs::write(run_dir.join("events.jsonl"), events_before_response).unwrap();
    let mut replacement = LocalRuntimeHost::start(cfg).expect("replacement Host");
    let recovered = replacement
        .resume(run_id, "stage this response", 2)
        .await
        .expect("recover staged response");

    assert_eq!(recovered.status, RunStatus::Succeeded);
    assert_eq!(recovered.output, "staged answer");
    assert_eq!(
        recovered.event_types,
        vec![
            "run.restored",
            "model.provider.selected",
            "model.output.delta",
            "run.succeeded",
        ]
    );
    let recovered_journal = current_route_journal(&journal_path);
    assert_eq!(recovered_journal["completed"], true);
    assert_eq!(recovered_journal["staged_events"], serde_json::json!([]));
}

#[tokio::test]
async fn route_freeze_filters_health_region_capability_and_cost_before_network_egress() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let mut excluded = Vec::new();
    for _ in 0..4 {
        excluded.push(spawn_must_not_run().await);
    }
    let success = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"eligible route\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (eligible_base, eligible_request, eligible_server) = spawn_response(200, success).await;
    let mut unhealthy = candidate(
        "unhealthy",
        ProviderProtocol::OpenAiCompatible,
        format!("{}/v1/chat/completions", excluded[0].0),
        1,
    );
    unhealthy.healthy = false;
    let mut wrong_region = candidate(
        "wrong-region",
        ProviderProtocol::OpenAiCompatible,
        format!("{}/v1/chat/completions", excluded[1].0),
        2,
    );
    wrong_region.region = "remote".into();
    let mut over_budget = candidate(
        "over-budget",
        ProviderProtocol::OpenAiCompatible,
        format!("{}/v1/chat/completions", excluded[2].0),
        3,
    );
    over_budget.cost_per_million_tokens_micros = 11;
    let missing_tool_use = candidate(
        "missing-tool-use",
        ProviderProtocol::OpenAiCompatible,
        format!("{}/v1/chat/completions", excluded[3].0),
        4,
    );
    let mut eligible = candidate(
        "eligible",
        ProviderProtocol::OpenAiCompatible,
        format!("{eligible_base}/v1/chat/completions"),
        5,
    );
    eligible.capabilities.insert(Capability::ToolUse);
    let mut cfg = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        vec![
            unhealthy,
            wrong_region,
            over_budget,
            missing_tool_use,
            eligible,
        ],
    );
    cfg.delegated_scopes.insert("agent:spawn".into());
    cfg.subagent_roles = vec![SubagentRole {
        name: "reviewer".into(),
        instructions: "Review only.".into(),
        delegated_scopes: BTreeSet::new(),
    }];
    let mut host = LocalRuntimeHost::start(cfg).expect("Host");

    let outcome = host
        .execute("select one eligible route")
        .await
        .expect("Run");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "eligible route");
    let request = eligible_request.await.expect("eligible request");
    assert!(
        request.body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    eligible_server.await.unwrap();
    for (_, calls, server) in excluded {
        server.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let route_dir = state
        .path()
        .join("runs")
        .join(outcome.run_id.to_string())
        .join("model-routes");
    let journal_path = std::fs::read_dir(route_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let records = committed_route_records(&journal_path);
    assert_eq!(
        records.len(),
        4,
        "a normal successful invocation needs only fence, staged response, observation and completion commits"
    );
    let journal = records
        .last()
        .and_then(|record| record.get("journal"))
        .cloned()
        .expect("current route journal");
    assert_eq!(journal["candidate_ids"], serde_json::json!(["eligible"]));
}
