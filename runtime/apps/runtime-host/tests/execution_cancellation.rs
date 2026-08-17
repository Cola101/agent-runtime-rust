//! Cancellation closures at the standalone Host's real execution boundaries.
//!
//! These tests deliberately use a real trusted shell process and real HTTP/MCP
//! sockets. A cancelled Future or a mocked executor would not prove that the
//! Runtime closes the resource or records the Kernel terminal state.

use agent_protocol::{RunBudget, RunStatus, RuntimeExecutionPolicySnapshot};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalMcpServerConfig, LocalMcpTransportConfig,
    LocalModelRoutingConfig, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent, SHELL_SCOPE,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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

fn tool_call_turn(name: &str, call_id: &str, arguments: serde_json::Value) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn spawn_provider(body: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider addr").port();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("provider request");
        let mut request = vec![0u8; 128 * 1024];
        let _ = socket.read(&mut request).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_counting_provider(
    first_body: String,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind counting provider");
    let port = listener.local_addr().expect("provider addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let turn = observed.fetch_add(1, Ordering::SeqCst);
            let body = if turn == 0 {
                first_body.clone()
            } else {
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"unexpected continuation\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()
            };
            tokio::spawn(async move {
                let mut request = vec![0u8; 128 * 1024];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        requests,
        handle,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReverseRequestAt {
    Discovery,
    ToolCall,
}

async fn spawn_reverse_request_mcp_server(
    at: ReverseRequestAt,
) -> (
    String,
    oneshot::Receiver<serde_json::Value>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind reverse-request MCP");
    let port = listener.local_addr().expect("reverse MCP addr").port();
    let (rejected_tx, rejected_rx) = oneshot::channel();
    let rejected = Arc::new(std::sync::Mutex::new(Some(rejected_tx)));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let rejected = Arc::clone(&rejected);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 128 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = request
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body)
                    .unwrap_or_default();
                let message: serde_json::Value =
                    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({}));

                if message["id"] == "reverse-http-1" && message.get("error").is_some() {
                    if let Some(sender) = rejected.lock().expect("rejection lock").take() {
                        let _ = sender.send(message);
                    }
                    let response =
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }

                let method = message["method"].as_str().unwrap_or_default();
                let is_reverse_boundary = match at {
                    ReverseRequestAt::Discovery => method == "tools/list",
                    ReverseRequestAt::ToolCall => method == "tools/call",
                };
                if is_reverse_boundary {
                    let request_id = message["id"].clone();
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    socket.write_all(header.as_bytes()).await.unwrap();
                    let reverse = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "reverse-http-1",
                        "method": if at == ReverseRequestAt::Discovery {
                            "roots/list"
                        } else {
                            "sampling/createMessage"
                        },
                        "params": {}
                    });
                    socket
                        .write_all(format!("data: {reverse}\n\n").as_bytes())
                        .await
                        .unwrap();
                    socket.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let result = if at == ReverseRequestAt::Discovery {
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {"tools": [{
                                "name": "search",
                                "description": "must not be accepted",
                                "inputSchema": {"type": "object"}
                            }]}
                        })
                    } else {
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {
                                "content": [{"type": "text", "text": "must not be accepted"}],
                                "isError": false
                            }
                        })
                    };
                    let _ = socket
                        .write_all(format!("data: {result}\n\n").as_bytes())
                        .await;
                    return;
                }

                let response_body = if method == "initialize" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "reverse-test", "version": "1"}
                        }
                    })
                } else if method == "tools/list" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"tools": [{
                            "name": "search",
                            "description": "Attempt an unnegotiated reverse request",
                            "inputSchema": {"type": "object"}
                        }]}
                    })
                } else {
                    serde_json::json!({})
                };
                let response_body = response_body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), rejected_rx, handle)
}

#[derive(Clone, Copy)]
enum BlockingMcpMethod {
    Initialize,
    ToolCall,
}

async fn spawn_blocking_mcp_server(
    blocked: BlockingMcpMethod,
) -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind MCP");
    let port = listener.local_addr().expect("MCP addr").port();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let started = std::sync::Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let closed = std::sync::Arc::new(std::sync::Mutex::new(Some(closed_tx)));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let started = std::sync::Arc::clone(&started);
            let closed = std::sync::Arc::clone(&closed);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 128 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let should_block = match blocked {
                    BlockingMcpMethod::Initialize => request.contains("\"method\":\"initialize\""),
                    BlockingMcpMethod::ToolCall => request.contains("\"method\":\"tools/call\""),
                };
                if should_block {
                    if let Some(sender) = started.lock().expect("started lock").take() {
                        let _ = sender.send(());
                    }
                    let mut tail = [0u8; 1024];
                    while socket.read(&mut tail).await.unwrap_or(0) != 0 {}
                    if let Some(sender) = closed.lock().expect("closed lock").take() {
                        let _ = sender.send(());
                    }
                    return;
                }

                let body = if request.contains("\"method\":\"initialize\"") {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "cancel-test", "version": "1"}
                        }
                    })
                } else if request.contains("\"method\":\"tools/list\"") {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"tools": [{
                            "name": "search",
                            "description": "Block until cancelled",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"query": {"type": "string"}},
                                "required": ["query"]
                            }
                        }]}
                    })
                } else {
                    serde_json::json!({})
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    (
        format!("http://127.0.0.1:{port}/mcp"),
        started_rx,
        closed_rx,
        handle,
    )
}

async fn spawn_lifecycle_mcp_server() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind lifecycle MCP");
    let port = listener.local_addr().expect("lifecycle MCP addr").port();
    let (started_tx, started_rx) = oneshot::channel();
    let (progress_tx, progress_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let started = std::sync::Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let progress = std::sync::Arc::new(std::sync::Mutex::new(Some(progress_tx)));
    let cancelled = std::sync::Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));
    let closed = std::sync::Arc::new(std::sync::Mutex::new(Some(closed_tx)));
    let active_call = std::sync::Arc::new(std::sync::Mutex::new(
        None::<(serde_json::Value, serde_json::Value)>,
    ));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let started = std::sync::Arc::clone(&started);
            let progress = std::sync::Arc::clone(&progress);
            let cancelled = std::sync::Arc::clone(&cancelled);
            let closed = std::sync::Arc::clone(&closed);
            let active_call = std::sync::Arc::clone(&active_call);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 128 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = request
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body)
                    .unwrap_or_default();
                let message: serde_json::Value =
                    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({}));
                let method = message["method"].as_str().unwrap_or_default();

                if method == "tools/call" {
                    let request_id = message["id"].clone();
                    let progress_token = message["params"]["_meta"]["progressToken"].clone();
                    *active_call.lock().expect("active call lock") =
                        Some((request_id, progress_token.clone()));
                    if let Some(sender) = started.lock().expect("started lock").take() {
                        let _ = sender.send(());
                    }
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    socket.write_all(header.as_bytes()).await.unwrap();
                    for (value, text) in [(1.0, "catalog searched"), (2.0, "result pending")] {
                        let frame = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": progress_token,
                                "progress": value,
                                "total": 3.0,
                                "message": text
                            }
                        });
                        socket
                            .write_all(format!("data: {frame}\n\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    socket.flush().await.unwrap();
                    if let Some(sender) = progress.lock().expect("progress lock").take() {
                        let _ = sender.send(());
                    }
                    let mut tail = [0u8; 1024];
                    while socket.read(&mut tail).await.unwrap_or(0) != 0 {}
                    if let Some(sender) = closed.lock().expect("closed lock").take() {
                        let _ = sender.send(());
                    }
                    return;
                }

                if method == "notifications/cancelled" {
                    let expected = active_call.lock().expect("active call lock").clone();
                    let matches_active = expected.is_some_and(|(request_id, _)| {
                        message["params"]["requestId"] == request_id
                            && message["params"]["reason"] == "run cancellation requested"
                    });
                    if matches_active
                        && let Some(sender) = cancelled.lock().expect("cancelled lock").take()
                    {
                        let _ = sender.send(());
                    }
                    let response =
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }

                let response_body = if method == "initialize" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "lifecycle-test", "version": "1"}
                        }
                    })
                } else if method == "tools/list" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"tools": [{
                            "name": "search",
                            "description": "Report progress then block",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"query": {"type": "string"}},
                                "required": ["query"]
                            }
                        }]}
                    })
                } else {
                    serde_json::json!({})
                };
                let response_body = response_body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (
        format!("http://127.0.0.1:{port}/mcp"),
        started_rx,
        progress_rx,
        cancelled_rx,
        closed_rx,
        handle,
    )
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    provider_endpoint: String,
    scopes: BTreeSet<String>,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Use an authorized Tool before answering.".into(),
        delegated_scopes: scopes,
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            provider_endpoint,
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
    }
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

async fn wait_for_process_exit(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn cancelling_a_running_non_idempotent_native_tool_reaps_it_and_preserves_uncertainty() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let marker = workspace_root.join("tool.started");
    let pid_file = workspace_root.join("tool.pid");
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "shell.exec",
        "call_shell_cancel",
        serde_json::json!({
            "command": "echo $$ > tool.pid; : > tool.started; sleep 3600"
        }),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace_root,
        provider_endpoint,
        BTreeSet::from([SHELL_SCOPE.to_owned()]),
    );
    local_config.trusted_workspace_tool = Some(trusted_tool);
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(local_config, cancellation.clone())
        .expect("host");
    let running = tokio::spawn(async move { host.execute("Run the long command.").await });

    wait_for_file(&marker).await;
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("tool pid")
        .trim()
        .parse()
        .expect("numeric tool pid");
    cancellation.cancel();
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled Tool Run hung")
        .expect("host task panicked")
        .expect("Tool cancellation must be a Run terminal state, not an execution error");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.indeterminate")
            .count(),
        1
    );
    assert!(
        wait_for_process_exit(pid).await,
        "the cancelled native Tool process survived"
    );
    provider.await.expect("provider turn");
}

#[tokio::test]
async fn duration_budget_reaps_a_running_non_idempotent_native_tool_and_preserves_uncertainty() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let marker = workspace_root.join("deadline-tool.started");
    let pid_file = workspace_root.join("deadline-tool.pid");
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "shell.exec",
        "call_shell_deadline",
        serde_json::json!({
            "command": "echo $$ > deadline-tool.pid; : > deadline-tool.started; sleep 3600"
        }),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace_root,
        provider_endpoint,
        BTreeSet::from([SHELL_SCOPE.to_owned()]),
    );
    local_config.trusted_workspace_tool = Some(trusted_tool);
    local_config.budget.max_duration_seconds = 1;
    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let running = tokio::spawn(async move { host.execute("Run the bounded command.").await });

    wait_for_file(&marker).await;
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("tool pid")
        .trim()
        .parse()
        .expect("numeric tool pid");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("duration-limited Tool Run hung")
        .expect("host task panicked")
        .expect("duration expiry must be a Run terminal state");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.indeterminate")
            .count(),
        1
    );
    assert!(
        wait_for_process_exit(pid).await,
        "the duration-limited native Tool process survived"
    );
    provider.await.expect("provider turn");
}

#[tokio::test]
async fn cancelling_an_inflight_unknown_mcp_call_closes_http_and_preserves_uncertainty() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, call_started, call_closed, mcp) =
        spawn_blocking_mcp_server(BlockingMcpMethod::ToolCall).await;
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "mcp:local/search",
        "call_mcp_cancel",
        serde_json::json!({"query": "runtime evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0061),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(local_config, cancellation.clone())
        .expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    timeout(Duration::from_secs(5), call_started)
        .await
        .expect("MCP Tool call did not start")
        .expect("MCP start signal dropped");
    cancellation.cancel();
    timeout(Duration::from_secs(5), call_closed)
        .await
        .expect("MCP HTTP request survived cancellation")
        .expect("MCP close signal dropped");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled MCP Tool Run hung")
        .expect("host task panicked")
        .expect("MCP cancellation must be a Run terminal state, not an execution error");
    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "run.indeterminate")
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("indeterminate cancellation must be checkpointed");
    assert_eq!(checkpoint.status, RunStatus::Indeterminate);
    let terminal = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "run.indeterminate")
        .expect("durable indeterminate event");
    assert_eq!(terminal.payload["effect"], "unknown");
    assert_eq!(terminal.payload["interrupted_by"], "cancellation");
    assert_eq!(terminal.payload["requested_status"], "cancelled");
    assert_eq!(terminal.payload["replay_safe"], false);
    provider.await.expect("provider turn");
    mcp.abort();
}

#[tokio::test]
async fn cancelling_an_mcp_call_sends_protocol_cancel_and_persists_bounded_progress() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, call_started, progress_sent, cancel_received, call_closed, mcp) =
        spawn_lifecycle_mcp_server().await;
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "mcp:local/search",
        "call_mcp_lifecycle",
        serde_json::json!({"query": "runtime lifecycle evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0065),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(local_config, cancellation.clone())
        .expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    timeout(Duration::from_secs(5), call_started)
        .await
        .expect("MCP Tool call did not start")
        .expect("MCP start signal dropped");
    timeout(Duration::from_secs(5), progress_sent)
        .await
        .expect("MCP server did not send progress")
        .expect("MCP progress signal dropped");
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();
    timeout(Duration::from_secs(5), cancel_received)
        .await
        .expect("Runtime did not send notifications/cancelled")
        .expect("MCP cancellation signal dropped");
    timeout(Duration::from_secs(5), call_closed)
        .await
        .expect("MCP HTTP request survived cancellation")
        .expect("MCP close signal dropped");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled MCP Tool Run hung")
        .expect("host task panicked")
        .expect("MCP cancellation must become a durable terminal state");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    let progress = events
        .iter()
        .filter(|event| event.event_type == "tool.execution.progress")
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[0].payload["progress"], 1.0);
    assert_eq!(progress[1].payload["progress"], 2.0);
    assert!(progress[0].sequence < progress[1].sequence);
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).unwrap();
    assert_eq!(checkpoint.status, RunStatus::Indeterminate);
    assert_eq!(checkpoint.sequence, events.last().unwrap().sequence);
    provider.await.expect("provider turn");
    mcp.abort();
}

#[tokio::test]
async fn cancelling_a_stdio_mcp_call_sends_protocol_cancel_and_persists_progress() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let call_marker = state.path().join("stdio.call");
    let cancel_marker = state.path().join("stdio.cancel");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp_server.sh");
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "mcp:local/search",
        "call_stdio_mcp_lifecycle",
        serde_json::json!({"query": "stdio lifecycle evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0066),
        name: "local".into(),
        transport: LocalMcpTransportConfig::Stdio {
            command: PathBuf::from("/bin/sh"),
            args: vec![fixture.display().to_string()],
            env: BTreeMap::from([
                ("MCP_CALL_MARKER".into(), call_marker.display().to_string()),
                (
                    "MCP_CANCEL_MARKER".into(),
                    cancel_marker.display().to_string(),
                ),
                ("MCP_REPORT_PROGRESS".into(), "1".into()),
                ("MCP_STALL_CALL".into(), "1".into()),
            ]),
            cwd: None,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(local_config, cancellation.clone())
        .expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    wait_for_file(&call_marker).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();
    wait_for_file(&cancel_marker).await;
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled stdio MCP Tool Run hung")
        .expect("host task panicked")
        .expect("stdio MCP cancellation must become a durable terminal state");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    let progress = events
        .iter()
        .filter(|event| event.event_type == "tool.execution.progress")
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].payload["message"], "stdio work started");
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).unwrap();
    assert_eq!(checkpoint.sequence, events.last().unwrap().sequence);
    provider.await.expect("provider turn");
}

#[tokio::test]
async fn http_mcp_reverse_request_during_discovery_fails_before_model_egress() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, rejected, mcp) =
        spawn_reverse_request_mcp_server(ReverseRequestAt::Discovery).await;
    let (provider_endpoint, provider_requests, provider) =
        spawn_counting_provider(String::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 1;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0069),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = timeout(
        Duration::from_secs(5),
        host.execute("Do not answer without the required Tool."),
    )
    .await
    .expect("reverse-request discovery hung")
    .expect("required MCP protocol violation must become a durable Run failure");

    assert_eq!(outcome.status, RunStatus::Failed);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    assert_eq!(events.last().unwrap().event_type, "run.failed");
    assert_eq!(
        events.last().unwrap().payload["kind"],
        "required_mcp_unavailable"
    );
    let rejection = timeout(Duration::from_secs(2), rejected)
        .await
        .expect("MCP client did not answer the discovery reverse request")
        .expect("reverse rejection signal dropped");
    assert_eq!(rejection["id"], "reverse-http-1");
    assert_eq!(rejection["error"]["code"], -32601);
    assert_eq!(
        provider_requests.load(Ordering::SeqCst),
        0,
        "a discovery-time roots request must not reach the model"
    );
    host.shutdown().await;
    provider.abort();
    mcp.abort();
}

#[tokio::test]
async fn stdio_mcp_reverse_request_during_discovery_fails_before_model_egress() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let rejection_marker = state.path().join("stdio.discovery-reverse-response.json");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp_server.sh");
    let (provider_endpoint, provider_requests, provider) =
        spawn_counting_provider(String::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 1;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0070),
        name: "local".into(),
        transport: LocalMcpTransportConfig::Stdio {
            command: PathBuf::from("/bin/sh"),
            args: vec![fixture.display().to_string()],
            env: BTreeMap::from([
                ("MCP_REVERSE_REQUEST_AT".into(), "list".into()),
                ("MCP_REVERSE_REQUEST_METHOD".into(), "roots/list".into()),
                (
                    "MCP_REVERSE_RESPONSE_MARKER".into(),
                    rejection_marker.display().to_string(),
                ),
            ]),
            cwd: None,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = timeout(
        Duration::from_secs(5),
        host.execute("Do not answer without the required Tool."),
    )
    .await
    .expect("stdio reverse-request discovery hung")
    .expect("required stdio MCP protocol violation must become a durable Run failure");

    assert_eq!(outcome.status, RunStatus::Failed);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    assert_eq!(events.last().unwrap().event_type, "run.failed");
    assert_eq!(
        events.last().unwrap().payload["kind"],
        "required_mcp_unavailable"
    );
    wait_for_file(&rejection_marker).await;
    let rejection: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&rejection_marker).expect("stdio discovery rejection marker"),
    )
    .expect("valid stdio discovery rejection");
    assert_eq!(rejection["id"], "reverse-stdio-1");
    assert_eq!(rejection["error"]["code"], -32601);
    assert_eq!(
        provider_requests.load(Ordering::SeqCst),
        0,
        "a discovery-time roots request must not reach the model"
    );
    host.shutdown().await;
    provider.abort();
}

#[tokio::test]
async fn http_mcp_unnegotiated_reverse_request_is_rejected_without_model_reentry() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, rejected, mcp) =
        spawn_reverse_request_mcp_server(ReverseRequestAt::ToolCall).await;
    let (provider_endpoint, provider_requests, provider) = spawn_counting_provider(tool_call_turn(
        "mcp:local/search",
        "call_http_reverse_request",
        serde_json::json!({"query": "reverse request evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0067),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = timeout(
        Duration::from_secs(5),
        host.execute("Search without granting MCP sampling authority."),
    )
    .await
    .expect("reverse-request Run hung")
    .expect("protocol violation must become a durable Run terminal");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    let rejection = timeout(Duration::from_secs(2), rejected)
        .await
        .expect("MCP client did not answer the reverse request")
        .expect("reverse rejection signal dropped");
    assert_eq!(rejection["id"], "reverse-http-1");
    assert_eq!(rejection["error"]["code"], -32601);
    assert_eq!(
        provider_requests.load(Ordering::SeqCst),
        1,
        "an unnegotiated sampling request must never re-enter the model"
    );
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool.execution.started")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "run.indeterminate")
    );
    assert!(!events.iter().any(|event| event.event_type == "tool.result"));
    provider.abort();
    mcp.abort();
}

#[tokio::test]
async fn stdio_mcp_unnegotiated_reverse_request_is_rejected_and_retires_the_session() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let rejection_marker = state.path().join("stdio.reverse-response.json");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp_server.sh");
    let (provider_endpoint, provider_requests, provider) = spawn_counting_provider(tool_call_turn(
        "mcp:local/search",
        "call_stdio_reverse_request",
        serde_json::json!({"query": "reverse request evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0068),
        name: "local".into(),
        transport: LocalMcpTransportConfig::Stdio {
            command: PathBuf::from("/bin/sh"),
            args: vec![fixture.display().to_string()],
            env: BTreeMap::from([
                (
                    "MCP_REVERSE_REQUEST_METHOD".into(),
                    "elicitation/create".into(),
                ),
                (
                    "MCP_REVERSE_RESPONSE_MARKER".into(),
                    rejection_marker.display().to_string(),
                ),
            ]),
            cwd: None,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = timeout(
        Duration::from_secs(5),
        host.execute("Search without granting MCP elicitation authority."),
    )
    .await
    .expect("stdio reverse-request Run hung")
    .expect("protocol violation must become a durable Run terminal");

    assert_eq!(outcome.status, RunStatus::Indeterminate);
    wait_for_file(&rejection_marker).await;
    let rejection: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&rejection_marker).expect("stdio reverse rejection marker"),
    )
    .expect("valid stdio reverse rejection");
    assert_eq!(rejection["id"], "reverse-stdio-1");
    assert_eq!(rejection["error"]["code"], -32601);
    assert_eq!(
        provider_requests.load(Ordering::SeqCst),
        1,
        "an unnegotiated elicitation request must never re-enter the model"
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).unwrap();
    assert_eq!(checkpoint.status, RunStatus::Indeterminate);
    provider.abort();
}

#[tokio::test]
async fn duration_budget_closes_an_inflight_unknown_mcp_call_and_preserves_uncertainty() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, call_started, call_closed, mcp) =
        spawn_blocking_mcp_server(BlockingMcpMethod::ToolCall).await;
    let (provider_endpoint, provider) = spawn_provider(tool_call_turn(
        "mcp:local/search",
        "call_mcp_deadline",
        serde_json::json!({"query": "runtime deadline evidence"}),
    ))
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.budget.max_duration_seconds = 1;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0063),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    timeout(Duration::from_secs(5), call_started)
        .await
        .expect("MCP Tool call did not start")
        .expect("MCP start signal dropped");
    timeout(Duration::from_secs(5), call_closed)
        .await
        .expect("MCP HTTP request survived the duration deadline")
        .expect("MCP close signal dropped");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("duration-limited MCP Tool Run hung")
        .expect("host task panicked")
        .expect("MCP duration expiry must be a Run terminal state");
    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "run.indeterminate")
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("indeterminate timeout must be checkpointed");
    assert_eq!(checkpoint.status, RunStatus::Indeterminate);
    let terminal = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "run.indeterminate")
        .expect("durable indeterminate event");
    assert_eq!(terminal.payload["effect"], "unknown");
    assert_eq!(terminal.payload["interrupted_by"], "duration_timeout");
    assert_eq!(terminal.payload["requested_status"], "timed_out");
    assert_eq!(terminal.payload["replay_safe"], false);
    provider.await.expect("provider turn");
    mcp.abort();
}

#[tokio::test]
async fn cancelling_mcp_discovery_closes_initialize_and_ends_the_run_as_cancelled() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, initialize_started, initialize_closed, mcp) =
        spawn_blocking_mcp_server(BlockingMcpMethod::Initialize).await;
    let (provider_endpoint, provider) = spawn_provider(String::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .per_server_timeout_ms = 30_000;
    local_config.runtime_policy.mcp_discovery.total_timeout_ms = 30_000;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0062),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(local_config, cancellation.clone())
        .expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    timeout(Duration::from_secs(5), initialize_started)
        .await
        .expect("MCP initialize did not start")
        .expect("MCP initialize signal dropped");
    cancellation.cancel();
    timeout(Duration::from_secs(5), initialize_closed)
        .await
        .expect("MCP initialize survived cancellation")
        .expect("MCP initialize close signal dropped");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("cancelled MCP discovery hung")
        .expect("host task panicked")
        .expect("discovery cancellation must be a Run terminal state");
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "run.cancelled")
    );
    assert!(
        !provider.is_finished(),
        "cancellation during discovery must not reach the model"
    );
    provider.abort();
    mcp.abort();
}

#[tokio::test]
async fn duration_budget_closes_mcp_discovery_before_the_model_is_invoked() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, initialize_started, initialize_closed, mcp) =
        spawn_blocking_mcp_server(BlockingMcpMethod::Initialize).await;
    let (provider_endpoint, provider) = spawn_provider(String::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("canonical"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.budget.max_duration_seconds = 1;
    local_config
        .runtime_policy
        .mcp_discovery
        .per_server_timeout_ms = 30_000;
    local_config.runtime_policy.mcp_discovery.total_timeout_ms = 30_000;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0064),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let running = tokio::spawn(async move { host.execute("Search before answering.").await });

    timeout(Duration::from_secs(5), initialize_started)
        .await
        .expect("MCP initialize did not start")
        .expect("MCP initialize signal dropped");
    timeout(Duration::from_secs(5), initialize_closed)
        .await
        .expect("MCP initialize survived the duration deadline")
        .expect("MCP initialize close signal dropped");
    let outcome = timeout(Duration::from_secs(5), running)
        .await
        .expect("duration-limited MCP discovery hung")
        .expect("host task panicked")
        .expect("discovery duration expiry must be a Run terminal state");
    assert_eq!(outcome.status, RunStatus::TimedOut);
    assert!(
        !provider.is_finished(),
        "duration expiry during discovery must not reach the model"
    );
    provider.abort();
    mcp.abort();
}
