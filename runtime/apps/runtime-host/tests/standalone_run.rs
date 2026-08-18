//! Minimal standalone acceptance for the local `runtime-host` (ADR-0035).
//!
//! Every test runs the real host against a real loopback provider with no Java
//! control plane, no PostgreSQL, no NATS, and no gRPC in the process.

use agent_protocol::{
    ContentPart, ContextCompactionPolicySnapshot, HistoryImport, HistoryImportSource,
    McpInputAction, McpInputResponse, Message, Role, RunBudget, RunStatus,
    RuntimeExecutionPolicySnapshot, RuntimeInvocationContext, SubagentRole,
    TOOL_RECONCILIATION_SCHEMA_VERSION, ToolEffect, ToolReconciliationCommand,
    ToolReconciliationDecision,
};
use agent_runtime_host::{
    LocalMcpInputResolution, LocalMcpLifecycleConfig, LocalMcpServerConfig,
    LocalMcpTransportConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalRuntimeError, LocalRuntimeHost, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use base64::Engine;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback OpenAI-compatible provider that serves a fixed number of turns.
/// `turns` are SSE bodies, served in order.
async fn spawn_provider(turns: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        for body in turns {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            // Drain the request; the assertions live in the host, not here.
            let _ = &buffer[..read];
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
        handle,
    )
}

async fn spawn_capturing_provider(
    answer: &str,
) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind capturing provider");
    let port = listener.local_addr().expect("provider addr").port();
    let answer = answer.to_owned();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("continuation request");
        let request = read_json_request(&mut socket).await;
        let body = text_turn(&answer);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        request
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_history_import_provider() -> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>)
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind history import provider");
    let port = listener.local_addr().expect("provider addr").port();
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["imported answer"] {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            requests.push(read_json_request(&mut socket).await);
            let body = text_turn(answer);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn read_json_request(socket: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let (header_end, content_length) = loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("read provider request");
        assert!(read > 0, "provider request closed before its headers");
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
    serde_json::from_slice(&request[header_end..header_end + content_length])
        .expect("provider request JSON")
}

fn tool_call_turn_for(id: &str, name: &str, query: &str, narrative: &str) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "content": narrative,
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::json!({"query": query}).to_string()
                    }
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn spawn_compaction_provider() -> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind compaction provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = vec![
        tool_call_turn_for(
            "call_compact_old",
            "mcp:compact/search",
            "old",
            "I will inspect the older evidence.",
        ),
        tool_call_turn_for(
            "call_compact_recent",
            "mcp:compact/search",
            "recent",
            "I will inspect the recent evidence.",
        ),
        text_turn("The older turn inspected the old evidence."),
        text_turn("final answer after compacted context"),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            else {
                break;
            };
            requests.push(read_json_request(&mut socket).await);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_recovering_compaction_provider()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind recovering compaction provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = [
        tool_call_turn_for(
            "call_recovery_old",
            "mcp:compact/search",
            "old",
            "I will inspect the older evidence before recovery.",
        ),
        tool_call_turn_for(
            "call_recovery_recent",
            "mcp:compact/search",
            "recent",
            "I will inspect the recent evidence before recovery.",
        ),
        String::new(),
        text_turn("The failed summary request retained the older evidence."),
        text_turn("recovered final answer after compaction"),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (turn, body) in turns.into_iter().enumerate() {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            else {
                break;
            };
            requests.push(read_json_request(&mut socket).await);
            let response = if turn == 2 {
                let error = r#"{"error":{"message":"compaction fixture unavailable"}}"#;
                format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{error}",
                    error.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_compaction_failover_primary()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind primary compaction provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = [
        Some(tool_call_turn_for(
            "call_failover_old",
            "mcp:compact/search",
            "old",
            "I will inspect the older failover evidence.",
        )),
        Some(tool_call_turn_for(
            "call_failover_recent",
            "mcp:compact/search",
            "recent",
            "I will inspect the recent failover evidence.",
        )),
        None,
        Some(text_turn("final answer after routed compaction")),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            else {
                break;
            };
            requests.push(read_json_request(&mut socket).await);
            let response = if let Some(body) = body {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let error = r#"{"error":{"message":"primary compaction unavailable"}}"#;
                format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{error}",
                    error.len()
                )
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_compaction_summary_fallback()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind compaction fallback");
    let port = listener.local_addr().expect("provider addr").port();
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
        else {
            return requests;
        };
        requests.push(read_json_request(&mut socket).await);
        let body = text_turn("The older routed turn inspected the old evidence.");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_compaction_mcp_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind compaction MCP");
    let port = listener.local_addr().expect("MCP addr").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed_calls = Arc::clone(&observed_calls);
            tokio::spawn(async move {
                let request = read_json_request(&mut socket).await;
                let body = match request["method"].as_str() {
                    Some("initialize") => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "compaction-test", "version": "1"}
                        }
                    }),
                    Some("tools/list") => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"tools": [{
                            "name": "search",
                            "description": "Return bounded compaction evidence",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"query": {"type": "string"}},
                                "required": ["query"]
                            }
                        }]}
                    }),
                    Some("tools/call") => {
                        let call = observed_calls.fetch_add(1, Ordering::SeqCst);
                        let (marker, fill) = if call == 0 {
                            ("old-evidence-", 'o')
                        } else {
                            ("recent-evidence-", 'r')
                        };
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("{marker}{}", fill.to_string().repeat(2_500))
                                }],
                                "isError": false
                            }
                        })
                    }
                    _ => serde_json::json!({}),
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), calls, handle)
}

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn tool_call_turn() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_local_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace.read_text\",\
     \"arguments\":\"{\\\"path\\\":\\\"README.txt\\\"}\"}}]}}]}\n\n\
     data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

fn parallel_workspace_read_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_first",
                        "type": "function",
                        "function": {
                            "name": "workspace.read_text",
                            "arguments": "{\"path\":\"FIRST.txt\"}"
                        }
                    },
                    {
                        "index": 1,
                        "id": "call_second",
                        "type": "function",
                        "function": {
                            "name": "workspace.read_text",
                            "arguments": "{\"path\":\"SECOND.txt\"}"
                        }
                    }
                ]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn spawn_parallel_tool_provider() -> (String, tokio::task::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind parallel Tool provider");
    let port = listener.local_addr().expect("provider addr").port();
    let handle = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first model request");
        let _ = read_json_request(&mut first).await;
        let body = parallel_workspace_read_turn();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        first.write_all(response.as_bytes()).await.unwrap();
        first.flush().await.unwrap();

        let (mut second, _) = listener.accept().await.expect("follow-up model request");
        let request = read_json_request(&mut second).await;
        let body = text_turn("parallel evidence accepted");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        second.write_all(response.as_bytes()).await.unwrap();
        second.flush().await.unwrap();
        request
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn timing_workspace_tool(root: &std::path::Path) -> PathBuf {
    let executable = root.join("timing-workspace-tool");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/python3
import json, sys, time
request = json.load(sys.stdin)
started = time.time_ns()
time.sleep(1.0 if request["tool_call"]["arguments"]["path"] == "FIRST.txt" else 0.1)
finished = time.time_ns()
json.dump({
    "tool_call_id": request["tool_call"]["id"],
    "binding_digest": request["binding_digest"],
    "is_error": False,
    "content": {
        "path": request["tool_call"]["arguments"]["path"],
        "started_ns": started,
        "finished_ns": finished
    }
}, sys.stdout)
"#,
    )
    .expect("write timing Tool");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("make timing Tool executable");
    }
    executable
}

fn uncertain_workspace_write_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_uncertain_write",
                    "type": "function",
                    "function": {
                        "name": "workspace.write_text",
                        "arguments": "{\"path\":\"side-effect.txt\",\"text\":\"applied\\n\"}"
                    }
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn slow_side_effect_tool(root: &std::path::Path) -> PathBuf {
    let executable = root.join("slow-side-effect-tool");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/python3
import json, os, sys, time
request = json.load(sys.stdin)
arguments = request["tool_call"]["arguments"]
with open(arguments["path"], "a", encoding="utf-8") as marker:
    marker.write(arguments["text"])
    marker.flush()
    os.fsync(marker.fileno())
time.sleep(2.0)
json.dump({
    "tool_call_id": request["tool_call"]["id"],
    "binding_digest": request["binding_digest"],
    "is_error": False,
    "content": {"written": True}
}, sys.stdout)
"#,
    )
    .expect("write slow side-effect Tool");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("make side-effect Tool executable");
    }
    executable
}

fn mcp_tool_call_turn() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_mcp_1\",\"type\":\"function\",\"function\":{\"name\":\"mcp:local/search\",\
     \"arguments\":\"{\\\"query\\\":\\\"runtime evidence\\\"}\"}}]}}]}\n\n\
     data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

fn subagent_tool_call_turn() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_review_1\",\"type\":\"function\",\"function\":{\"name\":\"agent.spawn\",\
     \"arguments\":\"{\\\"role\\\":\\\"reviewer\\\",\\\"input\\\":\\\"Review the migration evidence.\\\",\\\"max_tokens\\\":400,\\\"max_cost_cents\\\":30,\\\"max_duration_seconds\\\":20}\"}}]}}]}\n\n\
     data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

fn subagent_tool_call_turn_for(call_id: &str, input: &str) -> String {
    let arguments = serde_json::json!({
        "role": "reviewer",
        "input": input,
        "max_tokens": 400,
        "max_cost_cents": 30,
        "max_duration_seconds": 20
    })
    .to_string();
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
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

async fn spawn_subagent_aware_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = vec![
        (
            subagent_tool_call_turn(),
            vec![
                "agent.spawn",
                "reviewer",
                "Explain evidence before conclusions.",
            ],
            Vec::new(),
        ),
        (
            text_turn("Migration evidence is consistent."),
            vec![
                "Review evidence only.",
                "Review the migration evidence.",
                "max_tokens\":400",
            ],
            vec!["agent.spawn", "Explain evidence before conclusions."],
        ),
        (
            text_turn("Parent accepted the review."),
            vec![
                "call_review_1",
                "Migration evidence is consistent.",
                "subagent_run_id",
            ],
            Vec::new(),
        ),
    ];
    let handle = tokio::spawn(async move {
        for (body, expected, forbidden) in turns {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            let mut buffer = vec![0u8; 128 * 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            for needle in expected {
                assert!(
                    request.contains(needle),
                    "provider request did not contain {needle}: {request}"
                );
            }
            for needle in forbidden {
                assert!(
                    !request.contains(needle),
                    "child provider request unexpectedly contained {needle}: {request}"
                );
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_two_subagent_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = vec![
        subagent_tool_call_turn_for("call_review_a", "Review evidence A."),
        text_turn("Review A passed."),
        subagent_tool_call_turn_for("call_review_b", "Review evidence B."),
        text_turn("Review B passed."),
        text_turn("Both reviews passed."),
    ];
    let handle = tokio::spawn(async move {
        for body in turns {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            let mut buffer = vec![0u8; 128 * 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            if body.contains("Review B passed.") {
                assert!(request.contains("Review evidence B."));
            }
            if body.contains("Both reviews passed.") {
                assert!(request.contains("call_review_a"));
                assert!(request.contains("call_review_b"));
                assert!(request.contains("Review A passed."));
                assert!(request.contains("Review B passed."));
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_open_mcp_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind MCP");
    let port = listener.local_addr().expect("MCP addr").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed_calls = Arc::clone(&observed_calls);
            tokio::spawn(async move {
                let request = read_json_request(&mut socket).await;
                let method = request["method"].as_str().unwrap_or_default();
                let body = if method == "initialize" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "local-test", "version": "1"}
                        }
                    })
                } else if method == "tools/list" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"tools": [{
                            "name": "search",
                            "description": "Return local runtime evidence",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"query": {"type": "string"}},
                                "required": ["query"]
                            }
                        }]}
                    })
                } else if method == "tools/call" {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "content": [{"type": "text", "text": "local mcp evidence"}],
                            "isError": false
                        }
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
    (format!("http://127.0.0.1:{port}/mcp"), calls, handle)
}

async fn spawn_read_only_mcp_server() -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind read-only MCP");
    let port = listener.local_addr().expect("read-only MCP addr").port();
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_methods = Arc::clone(&methods);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed_methods = Arc::clone(&observed_methods);
            tokio::spawn(async move {
                let request = read_json_request(&mut socket).await;
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                observed_methods.lock().unwrap().push(method.clone());
                assert_ne!(method, "tools/list", "read-only server received tools/list");
                let result = match method.as_str() {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"resources": {}, "prompts": {}},
                        "serverInfo": {"name": "read-only-test", "version": "1"}
                    }),
                    "resources/list" => serde_json::json!({
                        "resources": [{
                            "uri": "kb://knowledge/runbook",
                            "name": "runbook",
                            "mimeType": "text/markdown"
                        }]
                    }),
                    "resources/read" => serde_json::json!({
                        "contents": [{
                            "uri": "kb://knowledge/runbook",
                            "text": "Runtime recovery requires the frozen owner epoch."
                        }]
                    }),
                    "resources/templates/list" => serde_json::json!({
                        "resourceTemplates": [{
                            "uriTemplate": "kb://knowledge/{name}",
                            "name": "knowledge"
                        }]
                    }),
                    "prompts/list" => serde_json::json!({
                        "prompts": [{
                            "name": "summarize",
                            "arguments": [{"name": "tone", "required": false}]
                        }]
                    }),
                    "prompts/get" => serde_json::json!({
                        "description": "resolved",
                        "messages": [{
                            "role": "user",
                            "content": {"type": "text", "text": "Summarize the runbook"}
                        }]
                    }),
                    "notifications/initialized" => serde_json::json!({}),
                    other => panic!("unexpected read-only MCP method {other}"),
                };
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result
                })
                .to_string();
                let status = if method == "notifications/initialized" {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), methods, handle)
}

fn runtime_mcp_tool_turn(calls: serde_json::Value) -> String {
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": calls}}]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn spawn_runtime_mcp_read_provider()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Runtime MCP provider");
    let port = listener
        .local_addr()
        .expect("Runtime MCP provider addr")
        .port();
    let turns = [
        runtime_mcp_tool_turn(serde_json::json!([
            {
                "index": 0,
                "id": "call-list-resources",
                "type": "function",
                "function": {"name": "list_mcp_resources", "arguments": "{\"server\":\"knowledge\"}"}
            },
            {
                "index": 1,
                "id": "call-list-templates",
                "type": "function",
                "function": {"name": "list_mcp_resource_templates", "arguments": "{\"server\":\"knowledge\"}"}
            },
            {
                "index": 2,
                "id": "call-list-prompts",
                "type": "function",
                "function": {"name": "list_mcp_prompts", "arguments": "{\"server\":\"knowledge\"}"}
            }
        ])),
        runtime_mcp_tool_turn(serde_json::json!([
            {
                "index": 0,
                "id": "call-read-resource",
                "type": "function",
                "function": {"name": "read_mcp_resource", "arguments": "{\"server\":\"knowledge\",\"uri\":\"kb://knowledge/runbook\"}"}
            },
            {
                "index": 1,
                "id": "call-get-prompt",
                "type": "function",
                "function": {"name": "get_mcp_prompt", "arguments": "{\"server\":\"knowledge\",\"name\":\"summarize\",\"arguments\":{\"tone\":\"short\"}}"}
            }
        ])),
        text_turn("The frozen runbook and low-authority prompt were inspected."),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let (mut socket, _) = listener.accept().await.expect("model request");
            requests.push(read_json_request(&mut socket).await);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_modern_mrtr_mcp_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind modern MCP");
    let port = listener.local_addr().expect("modern MCP addr").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_json_request(&mut socket).await;
            let method = request["method"].as_str().unwrap_or_default();
            let result = match method {
                "server/discover" => serde_json::json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}}
                }),
                "tools/list" => serde_json::json!({
                    "resultType": "complete",
                    "tools": [{
                        "name": "confirm_search",
                        "description": "Search only after explicit confirmation",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}},
                            "required": ["query"]
                        }
                    }]
                }),
                "tools/call" => {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    if request["params"].get("inputResponses").is_none() {
                        serde_json::json!({
                            "resultType": "input_required",
                            "requestState": "opaque-state-byte-exact",
                            "inputRequests": {
                                "confirmation": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "form",
                                        "message": "Confirm this search",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": {"confirmed": {"type": "boolean"}},
                                            "required": ["confirmed"]
                                        }
                                    }
                                }
                            }
                        })
                    } else {
                        assert_eq!(request["params"]["requestState"], "opaque-state-byte-exact");
                        assert_eq!(
                            request["params"]["inputResponses"]["confirmation"]["content"]["confirmed"],
                            true
                        );
                        serde_json::json!({
                            "resultType": "complete",
                            "content": [{"type": "text", "text": "confirmed modern evidence"}],
                            "isError": false
                        })
                    }
                }
                other => panic!("unexpected modern MCP method {other}"),
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), calls, handle)
}

async fn spawn_modern_url_mrtr_mcp_server()
-> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind modern URL MCP");
    let port = listener.local_addr().expect("modern URL MCP addr").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_json_request(&mut socket).await;
            let result = match request["method"].as_str().unwrap_or_default() {
                "server/discover" => serde_json::json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}}
                }),
                "tools/list" => serde_json::json!({
                    "resultType": "complete",
                    "tools": [{
                        "name": "authorize_search",
                        "description": "Search after browser authorization",
                        "inputSchema": {"type": "object"}
                    }]
                }),
                "tools/call" => {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    if request["params"].get("inputResponses").is_none() {
                        serde_json::json!({
                            "resultType": "input_required",
                            "requestState": "url-state-byte-exact",
                            "inputRequests": {
                                "authorization": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "url",
                                        "message": "Authorize the external search",
                                        "url": "https://example.invalid/authorize",
                                        "elicitationId": "authorization-1"
                                    }
                                }
                            }
                        })
                    } else {
                        assert_eq!(request["params"]["requestState"], "url-state-byte-exact");
                        assert_eq!(
                            request["params"]["inputResponses"]["authorization"]["action"],
                            "accept"
                        );
                        assert!(
                            request["params"]["inputResponses"]["authorization"]
                                .get("content")
                                .is_none(),
                            "URL secrets must never pass through the Runtime"
                        );
                        serde_json::json!({
                            "resultType": "complete",
                            "content": [{"type": "text", "text": "URL authorization completed"}],
                            "isError": false
                        })
                    }
                }
                other => panic!("unexpected modern URL MCP method {other}"),
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), calls, handle)
}

async fn spawn_side_effect_then_drop_mcp_server()
-> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ambiguous MCP");
    let port = listener.local_addr().expect("MCP addr").port();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_json_request(&mut socket).await;
            let method = request["method"].as_str().unwrap_or_default();
            if method == "notifications/initialized" {
                socket
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write initialized acknowledgement");
                continue;
            }
            if method == "tools/call" {
                observed_calls.fetch_add(1, Ordering::SeqCst);
                // The server has accepted the call and performed its observable
                // side effect. It then advertises a larger body than it sends,
                // forcing a real transport-level response-loss error.
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 256\r\nConnection: close\r\n\r\n{\"jsonrpc\":",
                    )
                    .await
                    .expect("write truncated MCP response");
                socket.flush().await.expect("flush truncated MCP response");
                continue;
            }
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "ambiguous-test", "version": "1"}
                }),
                "tools/list" => serde_json::json!({
                    "tools": [{
                        "name": "search",
                        "description": "Apply one observable remote side effect",
                        "annotations": {
                            "readOnlyHint": true,
                            "idempotentHint": true,
                            "destructiveHint": false
                        },
                        "inputSchema": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}},
                            "required": ["query"]
                        }
                    }]
                }),
                other => panic!("unexpected MCP method {other}"),
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write MCP response");
            socket.flush().await.expect("flush MCP response");
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), calls, handle)
}

async fn spawn_mcp_aware_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = vec![
        (mcp_tool_call_turn(), vec!["mcp:local/search"]),
        (
            text_turn("answer grounded by MCP"),
            vec!["call_mcp_1", "local mcp evidence"],
        ),
    ];
    let handle = tokio::spawn(async move {
        for (body, expected) in turns {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            let mut buffer = vec![0u8; 128 * 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
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
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_root_session_provider() -> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>)
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind root Session provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = [
        mcp_tool_call_turn(),
        text_turn("root first answer"),
        text_turn("source second answer"),
        text_turn("fork second answer"),
        text_turn("rollback second answer"),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let (mut socket, _) = listener.accept().await.expect("root Session request");
            requests.push(read_json_request(&mut socket).await);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_recovering_root_session_provider()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind recovering root Session provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = [
        Some(mcp_tool_call_turn()),
        Some(text_turn("root recovery first answer")),
        None,
        Some(text_turn("root recovery resumed answer")),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("recovering root Session request");
            requests.push(read_json_request(&mut socket).await);
            let response = if let Some(body) = body {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                let error = r#"{"error":{"message":"fixture unavailable"}}"#;
                format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{error}",
                    error.len()
                )
            };
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_terminal_recovery_provider()
-> (String, tokio::task::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind terminal recovery provider");
    let port = listener.local_addr().expect("provider addr").port();
    let turns = [
        mcp_tool_call_turn(),
        text_turn("terminal recovery first answer"),
        text_turn("terminal recovery second answer"),
    ];
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in turns {
            let (mut socket, _) = listener.accept().await.expect("terminal recovery request");
            requests.push(read_json_request(&mut socket).await);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        requests
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    endpoint: String,
    scopes: BTreeSet<String>,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Explain evidence before conclusions.".into(),
        delegated_scopes: scopes,
        subagent_roles: Vec::new(),
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
    }
}

#[test]
fn local_mcp_effect_override_for_an_unlisted_tool_is_rejected_at_startup() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        "http://127.0.0.1:1/v1/chat/completions".into(),
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: "http://127.0.0.1:1/mcp".into(),
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::from([("delete_everything".into(), ToolEffect::Pure)]),
        required: false,
    }];

    let error = match LocalRuntimeHost::start(local_config) {
        Ok(_) => panic!("an effect override outside the signed Tool allowlist must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, LocalRuntimeError::Configuration(_)));
}

fn damaged_history_import() -> HistoryImport {
    HistoryImport {
        schema_version: 1,
        source: HistoryImportSource::Truncated,
        messages: vec![
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "Inspect imported evidence.".into(),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_call_id: "call_orphan".into(),
                    content: serde_json::json!({"text": "orphan"}),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text {
                        text: "I started the read.".into(),
                    },
                    ContentPart::ToolCall {
                        tool_call_id: "call_imported".into(),
                        name: "workspace.read_text".into(),
                        arguments: serde_json::json!({"path": "EVIDENCE.txt"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "The old process stopped here.".into(),
                }],
            },
        ],
    }
}

#[tokio::test]
async fn local_host_runs_an_agent_to_a_terminal_state_without_any_control_plane() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, _provider) = spawn_provider(vec![text_turn("local answer")]).await;

    let mut host = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    ))
    .expect("local host starts");

    let outcome = host.execute("Summarize the workspace.").await.expect("run");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "local answer");
    assert!(
        outcome
            .event_types
            .first()
            .is_some_and(|e| e == "run.started"),
        "unexpected event order: {:?}",
        outcome.event_types
    );
    assert!(
        outcome.event_types.iter().any(|e| e == "run.succeeded"),
        "run never reached a terminal event: {:?}",
        outcome.event_types
    );
    assert!(
        outcome.checkpoint_path.is_file(),
        "the local Checkpoint must exist on disk so a restart can resume"
    );
}

/// The model-facing MCP read surface is owned by the Runtime rather than by a
/// remote MCP server. A server with no Tool authority can still expose bounded
/// Resources, Resource Templates, and Prompts under its separate read scope.
#[tokio::test]
async fn runtime_owned_mcp_read_tools_complete_a_real_agent_loop_without_remote_tool_authority() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, observed_methods, mcp_server) = spawn_read_only_mcp_server().await;
    let (provider_endpoint, provider) = spawn_runtime_mcp_read_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["mcp:read:knowledge".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0051),
        name: "knowledge".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::new(),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut host = LocalRuntimeHost::start(local_config).expect("read-only MCP host starts");
    let outcome = host
        .execute("Inspect the configured read-only MCP knowledge before answering.")
        .await
        .expect("Runtime-owned MCP read loop");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.output,
        "The frozen runbook and low-authority prompt were inspected."
    );
    assert!(outcome.pending_approval.is_none());
    assert!(outcome.pending_mcp_input.is_none());
    assert!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "tool.result")
            .count()
            >= 5,
        "each Runtime-owned MCP read must use the normal durable Tool path"
    );

    host.shutdown().await;
    let requests = provider.await.expect("provider transcript");
    assert_eq!(requests.len(), 3);
    let advertised = requests[0]["tools"]
        .as_array()
        .expect("Runtime-owned read tools advertised")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<BTreeSet<_>>();
    for name in [
        "list_mcp_resources",
        "read_mcp_resource",
        "list_mcp_resource_templates",
        "list_mcp_prompts",
        "get_mcp_prompt",
    ] {
        assert!(advertised.contains(name), "missing Runtime Tool {name}");
    }
    let list_results = requests[1]["messages"].to_string();
    assert!(list_results.contains("kb://knowledge/runbook"));
    assert!(list_results.contains("kb://knowledge/{name}"));
    assert!(list_results.contains("summarize"));
    let read_results = requests[2]["messages"].to_string();
    assert!(read_results.contains("frozen owner epoch"));
    assert!(read_results.contains("Summarize the runbook"));
    assert!(
        requests[2]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "system")
            .all(|message| !message.to_string().contains("Summarize the runbook")),
        "a remote MCP Prompt must remain low-authority Tool data"
    );

    let methods = observed_methods.lock().unwrap().clone();
    assert!(!methods.iter().any(|method| method == "tools/list"));
    for expected in [
        "resources/list",
        "resources/read",
        "resources/templates/list",
        "prompts/list",
        "prompts/get",
    ] {
        assert!(
            methods.iter().any(|method| method == expected),
            "MCP server never received {expected}: {methods:?}"
        );
    }
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn explicit_invocation_identity_reaches_execution_checkpoint_events_and_recovery() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_provider(vec![text_turn("tenant-bound answer")]).await;
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        endpoint,
        BTreeSet::new(),
    );
    let invocation = RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: uuid::Uuid::now_v7(),
        application_id: uuid::Uuid::now_v7(),
        workload_identity_id: uuid::Uuid::now_v7(),
        workspace_id: uuid::Uuid::now_v7(),
        agent_version_id: uuid::Uuid::now_v7(),
        model_policy_id: uuid::Uuid::now_v7(),
    };
    let run_id = uuid::Uuid::now_v7();
    let mut host = LocalRuntimeHost::start_for_invocation(local_config.clone(), invocation)
        .expect("explicit invocation host");

    let outcome = host
        .execute_as(run_id, "Prove the invocation boundary.")
        .await
        .expect("tenant-bound run");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    provider.await.expect("provider");

    let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).expect("events");
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| {
        event.tenant_id == invocation.tenant_id
            && event.application_id == invocation.application_id
            && event.workload_identity_id == invocation.workload_identity_id
            && event.workspace_id == invocation.workspace_id
    }));

    let checkpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&outcome.checkpoint_path).expect("checkpoint body"))
            .expect("checkpoint JSON");
    assert_eq!(
        checkpoint["checkpoint"]["tenant_id"],
        invocation.tenant_id.to_string()
    );
    let state_bytes = base64::engine::general_purpose::STANDARD
        .decode(checkpoint["checkpoint"]["state"].as_str().expect("state"))
        .expect("checkpoint state");
    let worker_state: serde_json::Value =
        serde_json::from_slice(&state_bytes).expect("worker checkpoint state");
    assert_eq!(
        worker_state["application_id"],
        invocation.application_id.to_string()
    );
    assert_eq!(
        worker_state["workload_identity_id"],
        invocation.workload_identity_id.to_string()
    );

    let mut wrong_application = invocation;
    wrong_application.application_id = uuid::Uuid::now_v7();
    let mut replacement = LocalRuntimeHost::start_for_invocation(local_config, wrong_application)
        .expect("replacement host");
    let error = replacement
        .resume(run_id, "Prove the invocation boundary.", 2)
        .await
        .expect_err("another application must not resume the Run");
    assert!(error.to_string().contains("checkpoint"));
}

/// The production break this catches is a Host that advertises multiple Tool
/// calls but still executes them serially, or writes their results back in
/// process completion order.
#[tokio::test]
async fn standalone_host_runs_bounded_pure_tools_concurrently_and_commits_source_order() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let trusted = tempfile::tempdir().expect("trusted Tool root");
    let executable = timing_workspace_tool(trusted.path());
    let (endpoint, provider) = spawn_parallel_tool_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    );
    local_config.trusted_workspace_tool = Some(executable);
    local_config
        .runtime_policy
        .tool_execution
        .max_concurrent_tools = 2;
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = host
        .execute("Read both files.")
        .await
        .expect("parallel Run");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parallel evidence accepted");
    let request = provider.await.expect("provider task");
    let tool_messages = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(
        tool_messages
            .iter()
            .map(|message| message["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["call_first", "call_second"]
    );
    let intervals = tool_messages
        .iter()
        .map(|message| {
            let content: serde_json::Value =
                serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
            (
                content["started_ns"].as_u64().unwrap(),
                content["finished_ns"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        intervals[0].0 < intervals[1].1 && intervals[1].0 < intervals[0].1,
        "the two real Tool processes never overlapped: {intervals:?}"
    );
}

/// The production break this catches is a replacement Host either losing the
/// already-finished later result or calling the model before retrying the
/// unfinished replay-safe prefix.
#[tokio::test]
async fn standalone_host_recovers_a_half_finished_parallel_tool_batch() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let trusted = tempfile::tempdir().expect("trusted Tool root");
    let executable = timing_workspace_tool(trusted.path());
    let (endpoint, provider) = spawn_parallel_tool_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    );
    local_config.trusted_workspace_tool = Some(executable);
    local_config
        .runtime_policy
        .tool_execution
        .max_concurrent_tools = 2;
    let first_config = local_config.clone();
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).expect("first host");
        host.execute("Read both files.").await
    });

    let mut staged_run = None;
    for _ in 0..400 {
        if let Ok(entries) = std::fs::read_dir(state.path().join("runs")) {
            for entry in entries.filter_map(Result::ok) {
                let Ok(run_id) = uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()) else {
                    continue;
                };
                let checkpoint_path = LocalRuntimeHost::checkpoint_path(state.path(), run_id);
                let Ok(checkpoint) = LocalRuntimeHost::load_checkpoint(&checkpoint_path) else {
                    continue;
                };
                let Ok(snapshot): Result<serde_json::Value, _> =
                    serde_json::from_slice(&checkpoint.state)
                else {
                    continue;
                };
                if snapshot["staged_ordered_tool_results"]["call_second"].is_object()
                    && snapshot["outstanding_tool_calls"]["call_first"].is_object()
                {
                    staged_run = Some(run_id);
                    break;
                }
            }
        }
        if staged_run.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let run_id = staged_run.expect("later Tool result never reached the durable staging ledger");
    running.abort();
    let _ = running.await;
    // The deliberately slow Pure process from the interrupted Host exits on
    // its own. Waiting avoids leaving a test process behind while the
    // replacement replays the same safe read.
    tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let outcome = replacement
        .resume(run_id, "Read both files.", 2)
        .await
        .expect("parallel batch recovery");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parallel evidence accepted");
    let request = provider.await.expect("provider task");
    assert_eq!(
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["call_first", "call_second"]
    );
}

/// The production break this catches is a replacement Host retrying a Tool
/// whose external write happened after `tool.execution.started` was persisted
/// but before its bound result reached the Checkpoint.
#[tokio::test]
async fn replacement_host_terminates_an_uncertain_side_effect_without_replaying_it() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let trusted = tempfile::tempdir().expect("trusted Tool root");
    let executable = slow_side_effect_tool(trusted.path());
    let (endpoint, provider) = spawn_provider(vec![uncertain_workspace_write_turn()]).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        endpoint,
        BTreeSet::from([agent_runtime_host::WORKSPACE_WRITE_SCOPE.to_owned()]),
    );
    local_config.trusted_workspace_tool = Some(executable);
    let first_config = local_config.clone();
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).expect("first host");
        host.execute("Write the durable side effect once.").await
    });

    let marker = workspace.path().join("side-effect.txt");
    let mut interrupted_run = None;
    for _ in 0..1_000 {
        if std::fs::read_to_string(&marker).is_ok_and(|content| content == "applied\n")
            && let Ok(entries) = std::fs::read_dir(state.path().join("runs"))
        {
            for entry in entries.filter_map(Result::ok) {
                let Ok(run_id) = uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()) else {
                    continue;
                };
                let checkpoint_path = LocalRuntimeHost::checkpoint_path(state.path(), run_id);
                let Ok(checkpoint) = LocalRuntimeHost::load_checkpoint(&checkpoint_path) else {
                    continue;
                };
                let Ok(snapshot): Result<serde_json::Value, _> =
                    serde_json::from_slice(&checkpoint.state)
                else {
                    continue;
                };
                if snapshot["started_tool_calls"]["call_uncertain_write"].is_object() {
                    interrupted_run = Some(run_id);
                    break;
                }
            }
        }
        if interrupted_run.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let run_id = interrupted_run.expect("side effect never crossed the durable ambiguity boundary");
    running.abort();
    let _ = running.await;
    provider.await.expect("provider task");
    tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;

    let (continuation_endpoint, continuation_provider) =
        spawn_capturing_provider("continued after reconciliation").await;
    let (not_applied_endpoint, not_applied_provider) =
        spawn_capturing_provider("continued after confirmed non-application").await;
    let mut not_applied_config = local_config.clone();
    not_applied_config.model_routing = LocalModelRoutingConfig::single_openai_compatible(
        not_applied_endpoint,
        "local-test-model",
        "local-test-key",
    );
    local_config.model_routing = LocalModelRoutingConfig::single_openai_compatible(
        continuation_endpoint,
        "local-test-model",
        "local-test-key",
    );
    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let outcome = replacement
        .resume(run_id, "Write the durable side effect once.", 2)
        .await
        .expect("ambiguous side effect must become a stable Run outcome");
    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert_eq!(
        outcome.event_types,
        vec!["run.restored", "run.indeterminate"]
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "applied\n");
    let terminal = LocalRuntimeHost::replay_events(state.path(), run_id, 0)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "run.indeterminate")
        .expect("indeterminate event");
    assert_eq!(terminal.payload["tool_call_id"], "call_uncertain_write");
    assert_eq!(terminal.payload["effect"], "non_idempotent");
    assert_eq!(terminal.payload["replay_safe"], false);
    assert_eq!(terminal.payload["reason"], "tool_outcome_unknown");

    let mut reconciliation = ToolReconciliationCommand {
        schema_version: TOOL_RECONCILIATION_SCHEMA_VERSION,
        reconciliation_id: uuid::Uuid::now_v7(),
        version: 1,
        tenant_id: agent_runtime_host::LOCAL_TENANT_ID,
        source_run_id: run_id,
        source_terminal_event_id: terminal.event_id,
        tool_call_id: "call_uncertain_write".into(),
        binding_digest: terminal.payload["binding_digest"]
            .as_str()
            .unwrap()
            .to_owned(),
        operator_id: "operator@example.test".into(),
        decision: ToolReconciliationDecision::Applied {
            content: serde_json::json!({"written":true}),
            is_error: false,
        },
        continuation_input: Some("Continue after the operator-confirmed write.".into()),
        issued_at: chrono::Utc::now(),
    };
    reconciliation.reconciliation_id = run_id;
    let collision = replacement
        .reconcile_tool_outcome(reconciliation.clone())
        .await
        .expect_err("a reconciliation Run id must not alias an existing source Run");
    assert!(
        collision
            .to_string()
            .contains("already belongs to another Run")
    );
    reconciliation.reconciliation_id = uuid::Uuid::now_v7();
    let reconciled = replacement
        .reconcile_tool_outcome(reconciliation.clone())
        .await
        .expect("operator reconciliation starts a continuation Run");
    let continuation = reconciled.continuation.as_ref().expect("continuation Run");
    assert_ne!(continuation.run_id, run_id);
    assert_eq!(continuation.run_id, reconciliation.reconciliation_id);
    assert_eq!(continuation.status, RunStatus::Succeeded);
    assert_eq!(continuation.output, "continued after reconciliation");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "applied\n");
    let request = continuation_provider.await.expect("continuation provider");
    let tool_result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "call_uncertain_write")
        .expect("operator-supplied Tool result reached the model");
    assert_eq!(tool_result["role"], "tool");

    let duplicate = replacement
        .reconcile_tool_outcome(reconciliation.clone())
        .await
        .expect("exact duplicate is idempotent");
    assert_eq!(duplicate, reconciled);
    let mut conflicting = reconciliation.clone();
    conflicting.decision = ToolReconciliationDecision::NotApplied;
    let conflict = replacement
        .reconcile_tool_outcome(conflicting)
        .await
        .unwrap_err();
    assert!(conflict.to_string().contains("version conflict"));
    assert_eq!(
        LocalRuntimeHost::load_checkpoint(&LocalRuntimeHost::checkpoint_path(state.path(), run_id))
            .unwrap()
            .status,
        RunStatus::Indeterminate
    );

    let mut unresolved = reconciliation;
    unresolved.reconciliation_id = uuid::Uuid::now_v7();
    unresolved.version = 1;
    unresolved.decision = ToolReconciliationDecision::Unresolved;
    unresolved.continuation_input = None;
    let mut second_operator =
        LocalRuntimeHost::start(not_applied_config).expect("second operator host");
    let unresolved_outcome = second_operator
        .reconcile_tool_outcome(unresolved.clone())
        .await
        .expect("unresolved evidence is durably recorded");
    assert!(unresolved_outcome.continuation.is_none());
    assert_eq!(
        second_operator
            .reconcile_tool_outcome(unresolved.clone())
            .await
            .expect("duplicate unresolved decision is idempotent"),
        unresolved_outcome
    );
    let mut not_applied = unresolved;
    not_applied.version = 2;
    not_applied.decision = ToolReconciliationDecision::NotApplied;
    not_applied.continuation_input =
        Some("Continue after the operator confirmed no write occurred.".into());
    let not_applied_outcome = second_operator
        .reconcile_tool_outcome(not_applied)
        .await
        .expect("a newer decision may resolve an earlier unresolved record");
    assert_eq!(
        not_applied_outcome.continuation.unwrap().output,
        "continued after confirmed non-application"
    );
    let not_applied_request = not_applied_provider.await.expect("not-applied provider");
    assert!(
        not_applied_request
            .to_string()
            .contains("operator_confirmed_not_applied"),
        "the model did not receive the operator's non-application evidence"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "applied\n");
    second_operator.shutdown().await;
    replacement.shutdown().await;
}

#[tokio::test]
async fn mcp_response_loss_after_remote_side_effect_is_indeterminate_and_never_replayed() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_provider(vec![mcp_tool_call_turn()]).await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_side_effect_then_drop_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = host
        .execute("Apply the remote MCP evidence once.")
        .await
        .expect("ambiguous MCP transport loss becomes a durable Run outcome");
    assert_eq!(outcome.status, RunStatus::Indeterminate);
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .expect("durable MCP events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool.execution.started")
            .count(),
        1
    );
    let terminal = events
        .iter()
        .find(|event| event.event_type == "run.indeterminate")
        .expect("ambiguous MCP terminal");
    assert_eq!(terminal.payload["tool_call_id"], "call_mcp_1");
    assert_eq!(terminal.payload["effect"], "unknown");
    assert_eq!(terminal.payload["replay_safe"], false);
    assert!(
        events.iter().all(|event| event.event_type != "tool.result"),
        "response loss was falsely converted into a completed Tool result"
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("terminal MCP checkpoint");
    assert_eq!(checkpoint.status, RunStatus::Indeterminate);

    host.shutdown().await;
    provider.await.expect("single model turn");
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn operator_frozen_idempotent_mcp_effect_continues_after_response_loss_without_replay() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_provider(vec![
        mcp_tool_call_turn(),
        text_turn("continued after the idempotent MCP failure"),
    ])
    .await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_side_effect_then_drop_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::from([("search".to_owned(), ToolEffect::Idempotent)]),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let outcome = host
        .execute("Apply the idempotent remote MCP evidence once.")
        .await
        .expect("operator-authorized replay-safe failure should remain model-visible");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "continued after the idempotent MCP failure");
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .expect("durable MCP events");
    let result = events
        .iter()
        .find(|event| event.event_type == "tool.result")
        .expect("replay-safe MCP failure must be returned to the model");
    assert_eq!(result.payload["is_error"], true);
    assert!(
        events
            .iter()
            .all(|event| event.event_type != "run.indeterminate"),
        "the frozen idempotent effect was ignored"
    );

    host.shutdown().await;
    provider.await.expect("model continuation turn");
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn root_session_fork_and_rollback_keep_immutable_tool_history_without_replay() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_root_session_provider().await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_open_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("host");

    let first = host
        .start_session("Read the root evidence with the Tool.")
        .await
        .expect("root Session starts");
    assert_eq!(first.run.status, RunStatus::Succeeded);
    assert_eq!(first.head.generation, 1);
    assert_eq!(first.head.turn_count, 1);

    let source = host
        .continue_session(
            first.head.session_id,
            first.head.branch_id,
            first.head.generation,
            "Continue the source branch.",
        )
        .await
        .expect("source continuation");
    assert_eq!(source.head.turn_count, 2);

    let fork = host
        .fork_session(
            source.head.session_id,
            source.head.branch_id,
            source.head.generation,
            1,
        )
        .expect("fork from completed prefix");
    assert_ne!(fork.branch_id, source.head.branch_id);
    assert_eq!(fork.generation, 1);
    assert_eq!(fork.turn_count, 1);
    let forked = host
        .continue_session(
            fork.session_id,
            fork.branch_id,
            fork.generation,
            "Continue only the fork.",
        )
        .await
        .expect("fork continuation");
    assert_eq!(forked.head.turn_count, 2);

    let rolled = host
        .rollback_session(
            source.head.session_id,
            source.head.branch_id,
            source.head.generation,
            1,
        )
        .expect("rollback source head");
    assert_eq!(rolled.generation, 2);
    assert_eq!(rolled.turn_count, 1);
    assert!(
        host.continue_session(
            rolled.session_id,
            rolled.branch_id,
            1,
            "A stale generation must not run.",
        )
        .await
        .is_err()
    );
    let rolled_forward = host
        .continue_session(
            rolled.session_id,
            rolled.branch_id,
            rolled.generation,
            "Continue after rollback.",
        )
        .await
        .expect("post-rollback continuation");
    assert_eq!(rolled_forward.head.turn_count, 2);
    assert_eq!(
        host.session_history(rolled.session_id, rolled.branch_id, 1)
            .expect("archived source generation")
            .len(),
        2
    );
    assert_eq!(
        host.session_history(rolled.session_id, rolled.branch_id, 2)
            .expect("current rolled generation")
            .len(),
        2
    );
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        1,
        "Fork and Rollback must not schedule the historical Tool call"
    );

    host.shutdown().await;
    let requests = provider.await.expect("provider transcript evidence");
    let request_text = requests
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>();
    assert!(request_text[2].contains("call_mcp_1"));
    assert!(request_text[3].contains("call_mcp_1"));
    assert!(!request_text[3].contains("source second answer"));
    assert!(request_text[4].contains("call_mcp_1"));
    assert!(!request_text[4].contains("source second answer"));

    let session_path = state
        .path()
        .join("sessions")
        .join(rolled.session_id.to_string())
        .join("session.json");
    let mut damaged: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
    damaged["branches"][rolled.branch_id.to_string()]["archived_generations"]
        .as_object_mut()
        .unwrap()
        .remove("1");
    std::fs::write(&session_path, serde_json::to_vec_pretty(&damaged).unwrap()).unwrap();
    assert!(
        host.session_head(rolled.session_id, rolled.branch_id)
            .is_err(),
        "removing an archived generation must corrupt the Session record instead of erasing history"
    );
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn root_session_checkpoint_recovery_keeps_the_head_and_never_replays_history_tools() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_recovering_root_session_provider().await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_open_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .model_routing
        .health_policy
        .max_same_provider_attempts = 2;
    local_config
        .model_routing
        .health_policy
        .consecutive_failure_threshold = 1;
    local_config.model_routing.health_policy.cooldown_ms = 1;
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut first_host = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let first = first_host
        .start_session("Read the durable root evidence.")
        .await
        .expect("first Session Turn");
    let failed = first_host
        .continue_session(
            first.head.session_id,
            first.head.branch_id,
            first.head.generation,
            "Continue after the recoverable provider failure.",
        )
        .await;
    assert!(matches!(failed, Err(LocalRuntimeError::Provider(_))));
    let parked = first_host
        .session_head(first.head.session_id, first.head.branch_id)
        .expect("active Session head remains durable");
    let active_run_id = parked.active_run_id.expect("failed Turn remains active");
    assert!(
        first_host
            .fork_session(parked.session_id, parked.branch_id, parked.generation, 1,)
            .is_err(),
        "an active Turn must fence Fork"
    );
    assert!(
        first_host
            .rollback_session(parked.session_id, parked.branch_id, parked.generation, 0,)
            .is_err(),
        "an active Turn must fence Rollback and any late result race"
    );
    first_host.shutdown().await;
    drop(first_host);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume(
            active_run_id,
            "Continue after the recoverable provider failure.",
            2,
        )
        .await
        .expect("Checkpoint-bound root Turn resumes");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    let recovered_head = replacement
        .session_head(first.head.session_id, first.head.branch_id)
        .expect("recovered Session head");
    assert_eq!(recovered_head.generation, 1);
    assert_eq!(recovered_head.turn_count, 2);
    assert_eq!(recovered_head.active_run_id, None);
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        1,
        "recovery must not execute the historical Tool again"
    );
    replacement.shutdown().await;

    let requests = provider.await.expect("provider recovery evidence");
    assert_eq!(requests.len(), 4);
    let restored_request = requests[3].to_string();
    assert!(restored_request.contains("call_mcp_1"));
    assert!(restored_request.contains("local mcp evidence"));
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn terminal_checkpoint_closes_the_session_head_commit_crash_window_without_model_replay() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_terminal_recovery_provider().await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_open_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let first = host
        .start_session("Read evidence before the commit-window test.")
        .await
        .expect("first Turn");
    let session_path = state
        .path()
        .join("sessions")
        .join(first.head.session_id.to_string())
        .join("session.json");
    let pre_second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
    let second_input = "Complete the Turn whose terminal checkpoint must win.";
    let second = host
        .continue_session(
            first.head.session_id,
            first.head.branch_id,
            first.head.generation,
            second_input,
        )
        .await
        .expect("second Turn");
    assert_eq!(second.run.status, RunStatus::Succeeded);
    let checkpoint = LocalRuntimeHost::load_checkpoint(&second.run.checkpoint_path).unwrap();
    assert_eq!(checkpoint.status, RunStatus::Succeeded);

    // Recreate only the narrow crash window: terminal event and Checkpoint are
    // durable, while the Session head file still contains the pre-commit head
    // plus its active Turn binding.
    let mut crashed = pre_second;
    crashed["branches"][first.head.branch_id.to_string()]["active_turn"] = serde_json::json!({
        "run_id": second.run.run_id,
        "generation": first.head.generation,
        "history_digest": first.head.history_digest,
        "input": second_input
    });
    std::fs::write(&session_path, serde_json::to_vec_pretty(&crashed).unwrap()).unwrap();
    host.shutdown().await;
    drop(host);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let recovered = replacement
        .resume(second.run.run_id, second_input, 2)
        .await
        .expect("terminal Session Checkpoint commits without another model request");
    assert_eq!(recovered.status, RunStatus::Succeeded);
    assert_eq!(recovered.output, "terminal recovery second answer");
    let head = replacement
        .session_head(first.head.session_id, first.head.branch_id)
        .expect("committed head");
    assert_eq!(head.turn_count, 2);
    assert_eq!(head.active_run_id, None);
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    replacement.shutdown().await;

    let requests = provider.await.expect("provider request count");
    assert_eq!(
        requests.len(),
        3,
        "terminal recovery must not invoke the model a fourth time"
    );
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn terminal_checkpoint_republishes_a_missing_terminal_event_before_session_commit() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_terminal_recovery_provider().await;
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_open_mcp_server().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::now_v7(),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let mut host = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let first = host
        .start_session("Read evidence before the terminal publication test.")
        .await
        .expect("first Turn");
    let session_path = state
        .path()
        .join("sessions")
        .join(first.head.session_id.to_string())
        .join("session.json");
    let pre_second: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
    let second_input = "Complete the Turn whose terminal event is not yet published.";
    let second = host
        .continue_session(
            first.head.session_id,
            first.head.branch_id,
            first.head.generation,
            second_input,
        )
        .await
        .expect("second Turn");
    assert_eq!(second.run.status, RunStatus::Succeeded);
    let checkpoint = LocalRuntimeHost::load_checkpoint(&second.run.checkpoint_path).unwrap();
    assert_eq!(checkpoint.status, RunStatus::Succeeded);

    // Recreate the earlier half of the actual commit order: the terminal
    // Checkpoint is durable, but the process died before appending its terminal
    // Event or advancing the Session head.
    let event_path = state
        .path()
        .join("runs")
        .join(second.run.run_id.to_string())
        .join("events.jsonl");
    let mut committed = std::fs::read_to_string(&event_path)
        .expect("completed second Turn event log")
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let terminal: serde_json::Value =
        serde_json::from_str(&committed.pop().expect("second Turn has a terminal event"))
            .expect("terminal event JSON");
    assert_eq!(terminal["type"], "run.succeeded");
    let valid_prefix = format!("{}\n", committed.join("\n"));
    std::fs::write(&event_path, &valid_prefix)
        .expect("remove only the uncommitted terminal publication");
    let mut crashed = pre_second;
    crashed["branches"][first.head.branch_id.to_string()]["active_turn"] = serde_json::json!({
        "run_id": second.run.run_id,
        "generation": first.head.generation,
        "history_digest": first.head.history_digest,
        "input": second_input
    });
    std::fs::write(&session_path, serde_json::to_vec_pretty(&crashed).unwrap()).unwrap();
    host.shutdown().await;
    drop(host);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let mut corrupted = committed.clone();
    let mut first_event: serde_json::Value =
        serde_json::from_str(&corrupted[0]).expect("first committed event JSON");
    first_event["workspace_id"] = serde_json::json!(uuid::Uuid::now_v7());
    corrupted[0] = serde_json::to_string(&first_event).unwrap();
    std::fs::write(&event_path, format!("{}\n", corrupted.join("\n")))
        .expect("inject an identity-valid JSON row for fail-closed recovery");
    assert!(
        replacement
            .resume(second.run.run_id, second_input, 2)
            .await
            .is_err(),
        "a foreign event prefix must not receive the terminal receipt"
    );
    std::fs::write(&event_path, valid_prefix).expect("restore the exact committed prefix");
    let recovered = replacement
        .resume(second.run.run_id, second_input, 2)
        .await
        .expect("terminal Checkpoint republishes its exact Event before Session commit");
    assert_eq!(recovered.status, RunStatus::Succeeded);
    assert_eq!(recovered.output, "terminal recovery second answer");
    let events = LocalRuntimeHost::replay_events(state.path(), second.run.run_id, 0)
        .expect("reconciled event log");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "run.succeeded")
            .count(),
        1,
        "terminal publication must be exactly once"
    );
    assert_eq!(
        events.last().map(|event| event.event_id),
        terminal["event_id"]
            .as_str()
            .and_then(|event_id| uuid::Uuid::parse_str(event_id).ok()),
        "recovery must republish the Checkpoint-bound terminal identity"
    );
    let head = replacement
        .session_head(first.head.session_id, first.head.branch_id)
        .expect("committed head");
    assert_eq!(head.turn_count, 2);
    assert_eq!(head.active_run_id, None);
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    replacement.shutdown().await;

    let requests = provider.await.expect("provider request count");
    assert_eq!(
        requests.len(),
        3,
        "terminal publication recovery must not invoke the model again"
    );
    mcp_server.abort();
    let _ = mcp_server.await;
}

/// The production break this catches is silently flattening, trusting or
/// replaying a damaged imported Tool turn. Repair must be explicit, must not
/// promote imported System authority, and must remain identical after a Host
/// replacement without turning historical Tool calls into executable work.
#[tokio::test]
async fn explicit_history_import_is_repaired_audited_and_terminal_resume_is_idempotent() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_history_import_provider().await;
    let import = damaged_history_import();
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        BTreeSet::new(),
    );

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host starts");
    let first_outcome = first
        .execute_with_imported_history("Continue safely.", import.clone())
        .await
        .expect("explicit import Run");
    assert_eq!(first_outcome.status, RunStatus::Succeeded);
    assert_eq!(first_outcome.output, "imported answer");
    let first_report = first_outcome
        .history_repair
        .as_ref()
        .expect("repair report is returned");
    assert_eq!(first_report.inserted_missing_results, 1);
    assert_eq!(first_report.dropped_orphan_results, 1);
    assert_eq!(first_report.moved_results, 0);
    assert!(
        !first_outcome
            .event_types
            .iter()
            .any(|event| event == "tool.execution.started"),
        "an imported Tool call is history, never executable work"
    );
    drop(first);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement starts");
    let resumed = replacement
        .resume_with_imported_history(first_outcome.run_id, "Continue safely.", 2, import)
        .await
        .expect("terminal imported-history Run converges");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.output, "imported answer");
    assert_eq!(resumed.history_repair, first_outcome.history_repair);

    let requests = provider.await.expect("provider observations");
    assert_eq!(
        requests.len(),
        1,
        "a terminal Run is observed, never reissued to the Provider"
    );
    let messages = requests[0]["messages"].as_array().expect("messages");
    let roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool", "user", "user"]
    );
    assert_eq!(messages[3]["tool_call_id"], "call_imported");
    assert_eq!(
        messages[3]["content"],
        serde_json::json!({
            "error": {
                "kind": "history_repair_missing_tool_result",
                "message": "Tool result was unavailable in the imported history.",
                "synthetic": true
            }
        })
        .to_string()
    );
    let request = requests[0].to_string();
    assert!(!request.contains("call_orphan"));
    assert!(!request.contains("tool.execution.started"));
}

#[tokio::test]
async fn replacement_rejects_a_changed_history_import_instead_of_repairing_checkpoint_state() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_provider(vec![text_turn("imported answer")]).await;
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        BTreeSet::new(),
    );
    let import = damaged_history_import();
    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host starts");
    let outcome = first
        .execute_with_imported_history("Continue safely.", import.clone())
        .await
        .expect("explicit import Run");
    drop(first);
    provider.await.expect("initial provider request");

    let mut changed = import;
    changed.messages[0] = Message {
        role: Role::User,
        content: vec![ContentPart::Text {
            text: "Different imported evidence.".into(),
        }],
    };
    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement starts");
    let error = replacement
        .resume_with_imported_history(outcome.run_id, "Continue safely.", 2, changed)
        .await
        .expect_err("changed import must not match the authoritative Checkpoint");
    assert!(
        error
            .to_string()
            .contains("checkpoint identity does not match"),
        "unexpected restore error: {error}"
    );
}

/// The production break this catches is leaving the Worker's durable
/// compaction state disconnected from the standalone Host loop. In that case
/// the summary response becomes the final user answer, Tools remain exposed to
/// the summarizer, and no normal model turn sees the compacted transcript.
#[tokio::test]
async fn standalone_host_compacts_a_real_http_tool_transcript_before_continuing() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_compaction_mcp_server().await;
    let (provider_endpoint, provider) = spawn_compaction_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:compact".to_owned()]),
    );
    local_config.runtime_policy.context_compaction = ContextCompactionPolicySnapshot {
        enabled: true,
        trigger_bytes: 4_096,
        retain_bytes: 1_024,
        max_summary_tokens: 256,
    };
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0058),
        name: "compact".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut host = LocalRuntimeHost::start(local_config).expect("local host starts");
    let outcome = host
        .execute("Compare the old and recent evidence before answering.")
        .await
        .expect("compacted Run");
    let requests = provider.await.expect("provider observations");

    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        requests.len(),
        4,
        "summary must be followed by a normal turn"
    );
    assert_eq!(requests[2]["max_tokens"], 256);
    assert!(
        requests[2].get("tools").is_none(),
        "the summarizer must not inherit executable Tools"
    );
    let summary_request = requests[2].to_string();
    assert!(summary_request.contains("call_compact_old"));
    assert!(summary_request.contains("old-evidence-"));
    assert!(!summary_request.contains("call_compact_recent"));
    assert!(!summary_request.contains("recent-evidence-"));

    let continued_request = requests[3].to_string();
    assert!(continued_request.contains("[Earlier conversation summary]"));
    assert!(continued_request.contains("The older turn inspected the old evidence."));
    assert!(continued_request.contains("call_compact_recent"));
    assert!(continued_request.contains("recent-evidence-"));
    assert!(!continued_request.contains("call_compact_old"));
    assert!(!continued_request.contains("old-evidence-"));
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "context.compacted"),
        "the durable event stream must expose compaction: {:?}",
        outcome.event_types
    );
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome
            .output
            .ends_with("final answer after compacted context")
    );
    assert!(
        !outcome
            .output
            .contains("The older turn inspected the old evidence."),
        "the internal summary must never leak into user-visible output"
    );

    mcp_server.abort();
}

/// The production break this catches is checkpointing only the transcript but
/// not the selected compaction boundary. A replacement Host would then either
/// replay completed Tools or summarize a different prefix after the provider
/// failed between durable preparation and summary completion.
#[tokio::test]
async fn replacement_host_retries_the_same_pending_compaction_without_replaying_tools() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_compaction_mcp_server().await;
    let (provider_endpoint, provider) = spawn_recovering_compaction_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:compact".to_owned()]),
    );
    local_config
        .model_routing
        .health_policy
        .max_same_provider_attempts = 2;
    local_config
        .model_routing
        .health_policy
        .consecutive_failure_threshold = 1;
    local_config.model_routing.health_policy.cooldown_ms = 1;
    local_config.runtime_policy.context_compaction = ContextCompactionPolicySnapshot {
        enabled: true,
        trigger_bytes: 4_096,
        retain_bytes: 1_024,
        max_summary_tokens: 256,
    };
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0059),
        name: "compact".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let run_id = uuid::Uuid::now_v7();
    let input = "Compare old and recent evidence, even if the summarizer needs recovery.";

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let error = first
        .execute_as(run_id, input)
        .await
        .expect_err("the first compaction request must fail");
    assert!(matches!(error, LocalRuntimeError::Provider(_)), "{error:?}");
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);
    drop(first);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let outcome = replacement
        .resume(run_id, input, 2)
        .await
        .expect("pending compaction resumes");
    let requests = provider.await.expect("provider observations");

    assert_eq!(requests.len(), 5);
    assert_eq!(requests[2]["messages"], requests[3]["messages"]);
    assert_eq!(requests[2]["max_tokens"], requests[3]["max_tokens"]);
    assert!(requests[2].get("tools").is_none());
    assert!(requests[3].get("tools").is_none());
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        2,
        "recovery must not replay a completed Tool"
    );
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        outcome
            .event_types
            .first()
            .is_some_and(|event| event == "run.restored")
    );
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "context.compacted")
    );
    assert!(
        outcome
            .output
            .ends_with("recovered final answer after compaction")
    );

    mcp_server.abort();
}

/// A summarizer is still a model invocation. It must use the same frozen,
/// auditable candidate chain as an ordinary Agent turn instead of silently
/// bypassing failover through the first configured adapter.
#[tokio::test]
async fn transcript_compaction_uses_the_same_safe_provider_failover_path() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_compaction_mcp_server().await;
    let (primary_endpoint, primary) = spawn_compaction_failover_primary().await;
    let (fallback_endpoint, fallback) = spawn_compaction_summary_fallback().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        primary_endpoint,
        BTreeSet::from(["tool:mcp:compact".to_owned()]),
    );
    local_config.runtime_policy.context_compaction = ContextCompactionPolicySnapshot {
        enabled: true,
        trigger_bytes: 4_096,
        retain_bytes: 1_024,
        max_summary_tokens: 256,
    };
    local_config
        .runtime_policy
        .model_failover
        .max_provider_attempts = 2;
    local_config.runtime_policy.model_failover.fallback_on =
        BTreeSet::from([agent_protocol::ModelErrorKind::Unavailable]);
    let mut primary_candidate = local_config.model_routing.candidates[0].clone();
    primary_candidate.id = "compaction-primary".into();
    primary_candidate.latency_ms = 10;
    let fallback_candidate = LocalProviderConfig {
        id: "compaction-fallback".into(),
        endpoint: fallback_endpoint,
        latency_ms: 20,
        ..primary_candidate.clone()
    };
    local_config.model_routing.candidates = vec![primary_candidate, fallback_candidate];
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0060),
        name: "compact".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut host = LocalRuntimeHost::start(local_config).expect("local host starts");
    let outcome = host
        .execute("Compare evidence through the routed summarizer.")
        .await
        .expect("compaction must fail over before emitting summary output");
    let primary_requests = primary.await.expect("primary observations");
    let fallback_requests = fallback.await.expect("fallback observations");

    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);
    assert_eq!(primary_requests.len(), 4);
    assert_eq!(fallback_requests.len(), 1);
    assert_eq!(fallback_requests[0]["max_tokens"], 256);
    assert!(fallback_requests[0].get("tools").is_none());
    assert!(fallback_requests[0].to_string().contains("old-evidence-"));
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "model.provider.failed")
    );
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "context.compacted")
    );
    assert!(
        outcome
            .output
            .ends_with("final answer after routed compaction")
    );
    assert!(!outcome.output.contains("The older routed turn"));

    mcp_server.abort();
}

#[tokio::test]
async fn standalone_parent_executes_an_authorized_role_subagent_and_receives_its_result() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_subagent_aware_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        BTreeSet::from(["agent:spawn".to_owned(), WORKSPACE_READ_SCOPE.to_owned()]),
    );
    local_config.subagent_roles = vec![SubagentRole {
        name: "reviewer".into(),
        instructions: "Review evidence only.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("local host starts");

    let outcome = host
        .execute("Review the migration through a reviewer.")
        .await
        .expect("parent and child run");
    provider.await.expect("all parent/child turns observed");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "Parent accepted the review.");
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "subagent.spawn.requested"),
        "parent never durably requested the child: {:?}",
        outcome.event_types
    );
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "subagent.result.received"),
        "child result never re-entered the parent Tool call: {:?}",
        outcome.event_types
    );
    let records = std::fs::read_dir(state.path().join("runs"))
        .expect("run state directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("checkpoint.json").is_file())
        .count();
    assert_eq!(
        records, 2,
        "parent and child must have independent durable run identities"
    );
}

#[tokio::test]
async fn standalone_parent_can_run_two_role_subagents_sequentially() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_two_subagent_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        BTreeSet::from(["agent:spawn".to_owned(), WORKSPACE_READ_SCOPE.to_owned()]),
    );
    local_config.subagent_roles = vec![SubagentRole {
        name: "reviewer".into(),
        instructions: "Review evidence only.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    }];
    let mut host = LocalRuntimeHost::start(local_config).expect("local host starts");

    let outcome = host
        .execute("Run two independent reviews.")
        .await
        .expect("two sequential child runs");
    provider.await.expect("all parent/child turns observed");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "Both reviews passed.");
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.result.received")
            .count(),
        2
    );
}

#[tokio::test]
async fn standalone_checkpoint_contains_the_runtime_policy_accepted_before_execution() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, _provider) = spawn_provider(vec![text_turn("local answer")]).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        BTreeSet::new(),
    );
    let mut policy = RuntimeExecutionPolicySnapshot::default();
    policy.tool_execution.timeout_ms = 1_234;
    local_config.runtime_policy = policy;
    let mut host = LocalRuntimeHost::start(local_config).unwrap();

    let outcome = host.execute("Answer locally.").await.unwrap();
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();

    assert_eq!(state["schema_version"], 27);
    assert_eq!(
        state["runtime_policy"]["tool_execution"]["timeout_ms"],
        1_234
    );
}

#[tokio::test]
async fn a_restarted_local_host_observes_a_terminal_run_from_its_checkpoint() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let (endpoint, provider) = spawn_provider(vec![text_turn("first answer")]).await;

    let mut first = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace_root.clone(),
        endpoint.clone(),
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    ))
    .expect("first host starts");
    let first_outcome = first
        .execute("Summarize the workspace.")
        .await
        .expect("run");
    drop(first);

    // A brand new host process, sharing only the local state root.
    let mut second = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace_root,
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    ))
    .expect("replacement host starts");
    let resumed = second
        .resume(first_outcome.run_id, "Summarize the workspace.", 2)
        .await
        .expect("observe the terminal local checkpoint");

    assert_eq!(resumed.run_id, first_outcome.run_id);
    assert_eq!(
        resumed.attempt_id, first_outcome.attempt_id,
        "terminal observation must not manufacture another attempt"
    );
    assert_eq!(resumed.event_types, first_outcome.event_types);
    assert_eq!(resumed.output, "first answer");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    provider.await.expect("one Provider request only");
}

/// The production break this catches is keeping standalone MCP behind the
/// external gRPC Gateway, or restoring before the exact catalog is reattached.
/// The first Run must execute a real MCP Tool; a fresh Host must then observe
/// the terminal Checkpoint without rediscovery, model replay, or Tool replay.
#[tokio::test]
async fn standalone_mcp_executes_and_recovers_without_an_external_gateway() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().unwrap();
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_open_mcp_server().await;
    let (provider_endpoint, provider) = spawn_mcp_aware_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace_root,
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0041),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: false,
    }];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host starts");
    let first_outcome = first
        .execute("Use local search before answering.")
        .await
        .expect("standalone MCP Run");
    assert_eq!(first_outcome.status, RunStatus::Succeeded);
    assert_eq!(first_outcome.output, "answer grounded by MCP");
    assert!(
        first_outcome
            .event_types
            .iter()
            .any(|event| event == "tool.result")
    );
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    drop(first);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host starts");
    let resumed = replacement
        .resume(
            first_outcome.run_id,
            "Use local search before answering.",
            2,
        )
        .await
        .expect("terminal MCP checkpoint converges");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.output, "answer grounded by MCP");
    assert_eq!(resumed.event_types, first_outcome.event_types);
    assert_eq!(
        mcp_calls.load(Ordering::SeqCst),
        1,
        "recovery must rebuild the frozen catalog without replaying a completed Tool call"
    );

    provider.await.expect("provider assertions");
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn modern_mcp_input_round_trip_survives_host_replacement() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_modern_mrtr_mcp_server().await;
    let (provider_endpoint, provider) = spawn_provider(vec![
        tool_call_turn_for(
            "call_modern_mrtr",
            "mcp:modern/confirm_search",
            "runtime evidence",
            "",
        ),
        text_turn("answer after durable MCP confirmation"),
    ])
    .await;
    let input = "Confirm the modern MCP search before answering.";
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from([
            "tool:mcp:modern".to_owned(),
            "mcp:elicitation:modern".to_owned(),
        ]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0090),
        name: "modern".into(),
        transport: LocalMcpTransportConfig::StreamableHttp2026 {
            endpoint: mcp_endpoint,
            elicitation: true,
        },
        tool_names: BTreeSet::from(["confirm_search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let parked = first.execute(input).await.expect("MRTR Run parks");
    assert_eq!(parked.status, RunStatus::Suspended);
    let pending = parked
        .pending_mcp_input
        .clone()
        .expect("bounded MCP input is returned to the caller");
    assert_eq!(pending.request_state, "opaque-state-byte-exact");
    assert_eq!(pending.round, 1);
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 1);
    drop(first);

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume_with_mcp_input(
            parked.run_id,
            input,
            2,
            LocalMcpInputResolution {
                input_id: pending.input_id,
                input_version: 1,
                binding_digest: pending.binding_digest,
                responses: BTreeMap::from([(
                    "confirmation".into(),
                    McpInputResponse {
                        action: McpInputAction::Accept,
                        content: Some(serde_json::json!({"confirmed": true})),
                        meta: None,
                    },
                )]),
            },
        )
        .await
        .expect("replacement resumes exact MCP round");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.output, "answer after durable MCP confirmation");
    assert!(resumed.pending_mcp_input.is_none());
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);
    let events = LocalRuntimeHost::replay_events(state.path(), parked.run_id, 0).unwrap();
    let event_types = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    for required in [
        "mcp.input.required",
        "mcp.input.resolved",
        "mcp.input.continuation.started",
        "tool.result",
        "run.succeeded",
    ] {
        assert!(event_types.contains(&required), "missing event {required}");
    }

    provider.await.expect("provider served both turns");
    mcp_server.abort();
    let _ = mcp_server.await;
}

#[tokio::test]
async fn modern_url_elicitation_survives_host_replacement_without_secret_content() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, mcp_calls, mcp_server) = spawn_modern_url_mrtr_mcp_server().await;
    let (provider_endpoint, provider) = spawn_provider(vec![
        tool_call_turn_for(
            "call_url_mrtr",
            "mcp:modern/authorize_search",
            "runtime evidence",
            "",
        ),
        text_turn("answer after URL authorization"),
    ])
    .await;
    let input = "Authorize the modern MCP search outside the Runtime.";
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from([
            "tool:mcp:modern".to_owned(),
            "mcp:elicitation:modern".to_owned(),
        ]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0092),
        name: "modern".into(),
        transport: LocalMcpTransportConfig::StreamableHttp2026 {
            endpoint: mcp_endpoint,
            elicitation: true,
        },
        tool_names: BTreeSet::from(["authorize_search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let parked = first.execute(input).await.expect("URL MRTR Run parks");
    let pending = parked.pending_mcp_input.clone().expect("pending URL input");
    assert_eq!(parked.status, RunStatus::Suspended);
    assert!(matches!(
        pending.requests.get("authorization"),
        Some(agent_protocol::McpElicitationRequest::Url {
            url,
            elicitation_id,
            ..
        }) if url == "https://example.invalid/authorize" && elicitation_id == "authorization-1"
    ));
    first.shutdown().await;

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume_with_mcp_input(
            parked.run_id,
            input,
            2,
            LocalMcpInputResolution {
                input_id: pending.input_id,
                input_version: 1,
                binding_digest: pending.binding_digest,
                responses: BTreeMap::from([(
                    "authorization".into(),
                    McpInputResponse {
                        action: McpInputAction::Accept,
                        content: None,
                        meta: None,
                    },
                )]),
            },
        )
        .await
        .expect("replacement resumes URL round");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.output, "answer after URL authorization");
    assert_eq!(mcp_calls.load(Ordering::SeqCst), 2);
    replacement.shutdown().await;
    provider.await.expect("provider served both turns");
    mcp_server.abort();
    let _ = mcp_server.await;
}

/// Codex's MCP 2026 client supports stateless MRTR over stdio as well as HTTP.
/// This exercises the shipped process transport and proves the opaque state is
/// sufficient to continue after the original host and MCP process are gone.
#[cfg(unix)]
#[tokio::test]
async fn modern_stdio_mcp_input_round_trip_survives_host_replacement() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let marker = fixture_state.path().join("calls");
    let request_log = fixture_state.path().join("requests.jsonl");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp_2026_server.sh");
    let transport: LocalMcpTransportConfig = serde_json::from_value(serde_json::json!({
        "type": "stdio2026",
        "command": "/bin/sh",
        "args": [script],
        "env": {"MCP_CALL_MARKER": marker, "MCP_REQUEST_LOG": request_log},
        "cwd": null,
        "elicitation": true
    }))
    .expect("modern stdio transport is supported");
    let (provider_endpoint, provider) = spawn_provider(vec![
        tool_call_turn_for(
            "call_modern_stdio",
            "mcp:modern/confirm_search",
            "runtime evidence",
            "",
        ),
        text_turn("answer after modern stdio confirmation"),
    ])
    .await;
    let input = "Confirm the modern stdio MCP search before answering.";
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from([
            "tool:mcp:modern".to_owned(),
            "mcp:elicitation:modern".to_owned(),
        ]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0091),
        name: "modern".into(),
        transport,
        tool_names: BTreeSet::from(["confirm_search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let parked = first.execute(input).await.unwrap_or_else(|error| {
        panic!(
            "modern stdio Run parks: {error}; requests={}",
            std::fs::read_to_string(&request_log).unwrap_or_default()
        )
    });
    assert_eq!(parked.status, RunStatus::Suspended);
    let pending = parked.pending_mcp_input.clone().expect("pending input");
    assert_eq!(pending.request_state, "stdio-state");
    first.shutdown().await;

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume_with_mcp_input(
            parked.run_id,
            input,
            2,
            LocalMcpInputResolution {
                input_id: pending.input_id,
                input_version: 1,
                binding_digest: pending.binding_digest,
                responses: BTreeMap::from([(
                    "approval".into(),
                    McpInputResponse {
                        action: McpInputAction::Accept,
                        content: Some(serde_json::json!({"confirmed": true})),
                        meta: None,
                    },
                )]),
            },
        )
        .await
        .expect("replacement resumes modern stdio round");
    assert_eq!(
        resumed.status,
        RunStatus::Succeeded,
        "outcome={resumed:?}; requests={}",
        std::fs::read_to_string(&request_log).unwrap_or_default()
    );
    assert_eq!(resumed.output, "answer after modern stdio confirmation");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "started\ncontinued\n"
    );
    replacement.shutdown().await;
    provider.await.expect("provider served both turns");
}

/// Cross-project compatibility gate against Codex's strict MCP 2026 stdio test
/// server. It is ignored in ordinary CI because the reference checkout is not
/// a dependency of this repository. Release evidence must use
/// `runtime/scripts/test-codex-mcp-2026-compat.sh`, which pins the upstream
/// commit and source digest before supplying `CODEX_MCP_2026_STDIO_SERVER`.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires the external Codex MCP 2026 stdio reference server"]
async fn codex_mcp_2026_stdio_server_completes_a_recoverable_agent_loop() {
    let binary = std::env::var_os("CODEX_MCP_2026_STDIO_SERVER")
        .map(PathBuf::from)
        .expect("CODEX_MCP_2026_STDIO_SERVER must name the external Codex fixture");
    let binary = binary.canonicalize().expect("Codex MCP fixture binary");
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_provider(vec![
        tool_call_turn_for("call_codex_stdio", "mcp:codex/echo", "evidence", ""),
        text_turn("answer after Codex MCP 2026 compatibility"),
    ])
    .await;
    let input = "Use the Codex MCP 2026 stdio reference Tool.";
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from([
            "tool:mcp:codex".to_owned(),
            "mcp:elicitation:codex".to_owned(),
        ]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0093),
        name: "codex".into(),
        transport: LocalMcpTransportConfig::Stdio2026 {
            command: binary,
            args: vec!["modern".into()],
            env: BTreeMap::new(),
            cwd: None,
            elicitation: true,
        },
        tool_names: BTreeSet::from(["echo".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let parked = first.execute(input).await.expect("Codex MCP Run parks");
    let pending = parked
        .pending_mcp_input
        .clone()
        .expect("Codex pending input");
    assert_eq!(parked.status, RunStatus::Suspended);
    assert_eq!(pending.request_state, "stdio-state");
    first.shutdown().await;

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume_with_mcp_input(
            parked.run_id,
            input,
            2,
            LocalMcpInputResolution {
                input_id: pending.input_id,
                input_version: 1,
                binding_digest: pending.binding_digest,
                responses: BTreeMap::from([(
                    "approval".into(),
                    McpInputResponse {
                        action: McpInputAction::Accept,
                        content: Some(serde_json::json!({"approved": true})),
                        meta: None,
                    },
                )]),
            },
        )
        .await
        .expect("Codex MCP continuation succeeds after replacement");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.output, "answer after Codex MCP 2026 compatibility");
    replacement.shutdown().await;
    provider.await.expect("provider served both turns");
}

/// Cross-implementation compatibility gate against the independently
/// maintained `mark3labs/mcp-go` protocol stack. The release script pins the
/// filesystem Server source and its Go module graph before supplying the
/// binary. The Agent may call only the read-only directory-authority Tool.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires the locked mark3labs/mcp-go filesystem server"]
async fn mcp_go_filesystem_server_completes_an_agent_loop() {
    let binary = std::env::var_os("MCP_GO_FILESYSTEM_SERVER")
        .map(PathBuf::from)
        .expect("MCP_GO_FILESYSTEM_SERVER must name the external Go server");
    let binary = binary.canonicalize().expect("mcp-go Server binary");
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let allowed_root = tempfile::tempdir().expect("read-only MCP authority root");
    let allowed_root = allowed_root
        .path()
        .canonicalize()
        .expect("canonical MCP authority root");
    let (provider_endpoint, provider) = spawn_provider(vec![
        runtime_mcp_tool_turn(serde_json::json!([{
            "index": 0,
            "id": "call_mcp_go_allowed_directories",
            "type": "function",
            "function": {
                "name": "mcp:mcp_go/list_allowed_directories",
                "arguments": "{}"
            }
        }])),
        text_turn("answer after independent mcp-go compatibility"),
    ])
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:mcp_go".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0095),
        name: "mcp_go".into(),
        transport: LocalMcpTransportConfig::StdioV20250326 {
            command: binary,
            args: vec![allowed_root.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
            cwd: None,
        },
        tool_names: BTreeSet::from(["list_allowed_directories".to_owned()]),
        tool_effect_overrides: BTreeMap::from([(
            "list_allowed_directories".to_owned(),
            ToolEffect::Pure,
        )]),
        required: true,
    }];

    let mut host = LocalRuntimeHost::start(local_config).expect("mcp-go host starts");
    let outcome = host
        .execute("Use the mcp-go directory-authority Tool before answering.")
        .await
        .expect("independent mcp-go Agent Loop succeeds");

    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "independent mcp-go outcome: {outcome:?}"
    );
    assert_eq!(
        outcome.output,
        "answer after independent mcp-go compatibility"
    );
    assert!(outcome.pending_approval.is_none());
    assert!(outcome.pending_mcp_input.is_none());
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "tool.result")
    );
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.succeeded")
            .count(),
        1
    );

    host.shutdown().await;
    provider.await.expect("provider served both turns");
}

/// Cross-project compatibility gate against the official MCP reference
/// implementation's Streamable HTTP transport. Release evidence must use
/// `runtime/scripts/test-mcp-streamable-http-compat.sh`, which installs the
/// fully locked server dependency graph into a temporary directory.
#[tokio::test]
#[ignore = "requires the locked external MCP Streamable HTTP reference server"]
async fn official_streamable_http_server_completes_an_agent_loop() {
    let endpoint = std::env::var("AGENT_RUNTIME_MCP_COMPAT_ENDPOINT")
        .expect("AGENT_RUNTIME_MCP_COMPAT_ENDPOINT must name the official reference server");
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (provider_endpoint, provider) = spawn_provider(vec![
        runtime_mcp_tool_turn(serde_json::json!([{
            "index": 0,
            "id": "call_official_http_echo",
            "type": "function",
            "function": {
                "name": "mcp:everything/echo",
                "arguments": serde_json::json!({"message": "agent runtime evidence"}).to_string()
            }
        }])),
        text_turn("answer after official Streamable HTTP MCP compatibility"),
    ])
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:everything".to_owned()]),
    );
    local_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0094),
        name: "everything".into(),
        transport: LocalMcpTransportConfig::StreamableHttp { endpoint },
        tool_names: BTreeSet::from(["echo".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];

    let mut host = LocalRuntimeHost::start(local_config).expect("official HTTP MCP host starts");
    let outcome = host
        .execute("Use the official Streamable HTTP MCP echo Tool before answering.")
        .await
        .expect("official Streamable HTTP MCP Agent Loop succeeds");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.output,
        "answer after official Streamable HTTP MCP compatibility"
    );
    assert!(outcome.pending_approval.is_none());
    assert!(outcome.pending_mcp_input.is_none());
    assert!(
        outcome
            .event_types
            .iter()
            .any(|event| event == "tool.result")
    );
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.succeeded")
            .count(),
        1
    );

    host.shutdown().await;
    provider.await.expect("provider served both turns");
}

/// The production break this catches is binding a Checkpoint only to the Tool
/// catalog digest. A different endpoint can advertise an identical catalog;
/// recovery must still reject it because the approved remote authority moved.
#[tokio::test]
async fn standalone_mcp_recovery_rejects_another_server_with_the_same_catalog() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (first_mcp_endpoint, _, first_mcp) = spawn_open_mcp_server().await;
    let (replacement_mcp_endpoint, _, replacement_mcp) = spawn_open_mcp_server().await;
    let (provider_endpoint, provider) = spawn_provider(vec![
        text_turn("first answer"),
        text_turn("must not resume"),
    ])
    .await;
    let mut first_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    first_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0042),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: first_mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: false,
    }];
    let mut first = LocalRuntimeHost::start(first_config.clone()).unwrap();
    let outcome = first
        .execute("Answer with the configured tools.")
        .await
        .unwrap();
    drop(first);

    let mut replacement_config = first_config;
    replacement_config.mcp_servers[0].server_id =
        uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0043);
    replacement_config.mcp_servers[0].transport = LocalMcpTransportConfig::StreamableHttp {
        endpoint: replacement_mcp_endpoint,
    };
    let mut replacement = LocalRuntimeHost::start(replacement_config).unwrap();
    let error = replacement
        .resume(outcome.run_id, "Answer with the configured tools.", 2)
        .await
        .expect_err("a different MCP authority must not inherit the old Checkpoint");
    assert!(matches!(error, LocalRuntimeError::Checkpoint(_)));

    provider.abort();
    first_mcp.abort();
    replacement_mcp.abort();
    let _ = provider.await;
    let _ = first_mcp.await;
    let _ = replacement_mcp.await;
}

#[tokio::test]
async fn standalone_mcp_recovery_rejects_effect_policy_drift_with_the_same_catalog() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, _, mcp_server) = spawn_open_mcp_server().await;
    let (provider_endpoint, provider) = spawn_provider(vec![
        text_turn("first answer"),
        text_turn("must not resume under changed effect policy"),
    ])
    .await;
    let mut first_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    first_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0046),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: false,
    }];
    let mut first = LocalRuntimeHost::start(first_config.clone()).unwrap();
    let outcome = first
        .execute("Answer with the frozen MCP policy.")
        .await
        .unwrap();
    drop(first);

    let mut replacement_config = first_config;
    replacement_config.mcp_servers[0].tool_effect_overrides =
        BTreeMap::from([("search".to_owned(), ToolEffect::Idempotent)]);
    let mut replacement = LocalRuntimeHost::start(replacement_config).unwrap();
    let error = replacement
        .resume(outcome.run_id, "Answer with the frozen MCP policy.", 2)
        .await
        .expect_err("effect policy drift must not inherit the old Checkpoint");
    assert!(
        error
            .to_string()
            .contains("checkpoint identity does not match"),
        "policy drift failed at the wrong boundary: {error}"
    );

    provider.abort();
    mcp_server.abort();
    let _ = provider.await;
    let _ = mcp_server.await;
}

/// Required/optional is part of the accepted capability contract, not a UI
/// hint. If it is omitted from the Checkpoint authority binding, a recovered
/// Run can silently change from degraded-capable to fail-closed (or vice versa).
#[tokio::test]
async fn standalone_mcp_recovery_rejects_required_policy_drift_before_model_egress() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (mcp_endpoint, _calls, mcp) = spawn_open_mcp_server().await;
    let (provider_endpoint, provider) =
        spawn_provider(vec![mcp_tool_call_turn(), text_turn("first run completed")]).await;
    let mut first_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    first_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0045),
        name: "local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: mcp_endpoint,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: false,
    }];

    let mut first = LocalRuntimeHost::start(first_config.clone()).unwrap();
    let outcome = first
        .execute("Use the configured MCP Tool.")
        .await
        .expect("first Run");
    first.shutdown().await;
    provider.await.expect("first model loop only");

    first_config.mcp_servers[0].required = true;
    let mut replacement = LocalRuntimeHost::start(first_config).unwrap();
    let error = replacement
        .resume(outcome.run_id, "Use the configured MCP Tool.", 2)
        .await
        .expect_err("required policy drift must reject the Checkpoint");
    assert!(
        error
            .to_string()
            .contains("checkpoint identity does not match"),
        "drift must fail at Checkpoint identity, got {error}"
    );
    replacement.shutdown().await;
    mcp.abort();
    let _ = mcp.await;
}

fn stdio_mcp_config(
    call_marker: PathBuf,
    grandchild_pid_file: PathBuf,
    stall_list: bool,
    stall_initialize: bool,
) -> LocalMcpServerConfig {
    let mut env = BTreeMap::from([
        (
            "MCP_CALL_MARKER".to_owned(),
            call_marker.to_string_lossy().into_owned(),
        ),
        (
            "MCP_GRANDCHILD_PID_FILE".to_owned(),
            grandchild_pid_file.to_string_lossy().into_owned(),
        ),
    ]);
    if stall_list {
        env.insert("MCP_STALL_LIST".to_owned(), "1".to_owned());
    }
    if stall_initialize {
        env.insert("MCP_STALL_INITIALIZE".to_owned(), "1".to_owned());
    }
    LocalMcpServerConfig {
        server_id: uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0044),
        name: "local".into(),
        transport: LocalMcpTransportConfig::Stdio {
            command: PathBuf::from("/bin/sh"),
            args: vec![
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/stdio_mcp_server.sh")
                    .to_string_lossy()
                    .into_owned(),
            ],
            env,
            cwd: None,
        },
        tool_names: BTreeSet::from(["search".to_owned()]),
        tool_effect_overrides: BTreeMap::new(),
        required: false,
    }
}

#[test]
fn stdio_session_capacity_covers_frozen_discovery_concurrency() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        "http://127.0.0.1:1/v1/chat/completions".into(),
        BTreeSet::from(["tool:mcp:first".to_owned(), "tool:mcp:second".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_concurrent_servers = 2;
    local_config.mcp_lifecycle.max_sessions = 1;
    let first = stdio_mcp_config(
        state.path().join("first.calls"),
        state.path().join("first.pid"),
        false,
        false,
    );
    let mut second = stdio_mcp_config(
        state.path().join("second.calls"),
        state.path().join("second.pid"),
        false,
        false,
    );
    second.server_id = uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0045);
    second.name = "second".into();
    local_config.mcp_servers = vec![first, second];

    assert!(matches!(
        LocalRuntimeHost::start(local_config),
        Err(LocalRuntimeError::Configuration(message))
            if message == "local MCP lifecycle limits are invalid"
    ));
}

fn make_stdio_startup_flaky(
    server: &mut LocalMcpServerConfig,
    start_marker: &std::path::Path,
    grandchild_pid_log: &std::path::Path,
    failed_attempts: u8,
) {
    let LocalMcpTransportConfig::Stdio { env, .. } = &mut server.transport else {
        panic!("flaky startup fixture requires stdio")
    };
    env.insert(
        "MCP_START_MARKER".into(),
        start_marker.to_string_lossy().into_owned(),
    );
    env.insert(
        "MCP_GRANDCHILD_PID_LOG".into(),
        grandchild_pid_log.to_string_lossy().into_owned(),
    );
    env.insert(
        "MCP_FAIL_INITIALIZE_ATTEMPTS".into(),
        failed_attempts.to_string(),
    );
}

fn process_exists(pid: u32) -> bool {
    StdCommand::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn wait_for_process_exit(pid: u32) -> bool {
    for _ in 0..100 {
        if !process_exists(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

fn read_pid(path: &std::path::Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("stdio fixture records its grandchild")
        .trim()
        .parse()
        .expect("grandchild pid")
}

fn read_pids(path: &std::path::Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .expect("stdio fixture records every grandchild")
        .lines()
        .map(|line| line.parse().expect("grandchild pid"))
        .collect()
}

/// The production break this catches is reporting retry support while the
/// first failed initialize still ends discovery. Only the safe catalog phase
/// may retry; the eventual Tool call must still execute exactly once.
#[cfg(unix)]
#[tokio::test]
async fn required_stdio_mcp_recovers_within_its_frozen_retry_budget() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let call_marker = fixture_state.path().join("calls");
    let start_marker = fixture_state.path().join("starts");
    let pid_log = fixture_state.path().join("grandchildren");
    let (provider_endpoint, provider) = spawn_provider(vec![
        mcp_tool_call_turn(),
        text_turn("answer after MCP reconnect"),
    ])
    .await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 2;
    local_config
        .runtime_policy
        .mcp_discovery
        .initial_retry_backoff_ms = 10;
    let mut server = stdio_mcp_config(
        call_marker.clone(),
        fixture_state.path().join("latest.pid"),
        false,
        false,
    );
    server.required = true;
    make_stdio_startup_flaky(&mut server, &start_marker, &pid_log, 1);
    local_config.mcp_servers = vec![server];

    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let outcome = host
        .execute("Use the required local search Tool.")
        .await
        .expect("second startup attempt should recover");
    assert_eq!(outcome.output, "answer after MCP reconnect");
    assert_eq!(
        std::fs::read_to_string(&start_marker).unwrap(),
        "started\nstarted\n"
    );
    assert_eq!(std::fs::read_to_string(&call_marker).unwrap(), "called\n");
    assert_eq!(outcome.mcp_servers.len(), 1);
    assert_eq!(outcome.mcp_servers[0].attempts, 2);
    assert_eq!(
        outcome.mcp_servers[0].health,
        agent_runtime_worker::McpServerHealth::Ready
    );
    host.shutdown().await;
    for pid in read_pids(&pid_log) {
        assert!(
            wait_for_process_exit(pid).await,
            "retry child {pid} survived"
        );
    }
    provider.await.expect("provider assertions");
}

/// A required dependency is part of the Agent definition. Letting the model
/// continue without it changes the Run's capabilities and can fabricate a
/// plausible answer, so all bounded attempts must fail before model egress.
#[cfg(unix)]
#[tokio::test]
async fn unavailable_required_stdio_mcp_fails_before_model_egress() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let start_marker = fixture_state.path().join("starts");
    let pid_log = fixture_state.path().join("grandchildren");
    let (provider_endpoint, provider) = spawn_provider(Vec::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 2;
    local_config
        .runtime_policy
        .mcp_discovery
        .initial_retry_backoff_ms = 10;
    let mut server = stdio_mcp_config(
        fixture_state.path().join("calls"),
        fixture_state.path().join("latest.pid"),
        false,
        false,
    );
    server.required = true;
    make_stdio_startup_flaky(&mut server, &start_marker, &pid_log, 9);
    local_config.mcp_servers = vec![server];

    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let outcome = host
        .execute("Do not answer without the required Tool.")
        .await
        .expect("required server exhaustion is a terminal Run outcome");
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.mcp_servers.len(), 1);
    assert_eq!(
        outcome.mcp_servers[0].health,
        agent_runtime_worker::McpServerHealth::Unavailable
    );
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .expect("required MCP failure events");
    assert_eq!(
        events.last().map(|event| event.event_type.as_str()),
        Some("run.failed")
    );
    assert_eq!(
        events.last().unwrap().payload["kind"],
        "required_mcp_unavailable"
    );
    assert_eq!(
        std::fs::read_to_string(&start_marker).unwrap(),
        "started\nstarted\n"
    );
    host.shutdown().await;
    for pid in read_pids(&pid_log) {
        assert!(
            wait_for_process_exit(pid).await,
            "failed child {pid} survived"
        );
    }
    provider.await.expect("the model must not be called");
}

/// Optional failure is not silent success: the Run may continue, but its
/// observable outcome must say which capability was missing and how many safe
/// discovery attempts were consumed.
#[cfg(unix)]
#[tokio::test]
async fn unavailable_optional_stdio_mcp_is_reported_while_the_run_continues() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let start_marker = fixture_state.path().join("starts");
    let pid_log = fixture_state.path().join("grandchildren");
    let (provider_endpoint, provider) =
        spawn_provider(vec![text_turn("answer without optional MCP")]).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 2;
    local_config
        .runtime_policy
        .mcp_discovery
        .initial_retry_backoff_ms = 10;
    let mut server = stdio_mcp_config(
        fixture_state.path().join("calls"),
        fixture_state.path().join("latest.pid"),
        false,
        false,
    );
    make_stdio_startup_flaky(&mut server, &start_marker, &pid_log, 9);
    local_config.mcp_servers = vec![server];

    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let outcome = host
        .execute("Answer even if the optional Tool is offline.")
        .await
        .expect("optional server must not reject the Run");
    assert_eq!(outcome.output, "answer without optional MCP");
    assert_eq!(outcome.mcp_servers.len(), 1);
    assert!(!outcome.mcp_servers[0].required);
    assert_eq!(outcome.mcp_servers[0].attempts, 2);
    assert_eq!(
        outcome.mcp_servers[0].health,
        agent_runtime_worker::McpServerHealth::Unavailable
    );
    assert!(outcome.mcp_servers[0].error.is_some());
    host.shutdown().await;
    for pid in read_pids(&pid_log) {
        assert!(
            wait_for_process_exit(pid).await,
            "optional child {pid} survived"
        );
    }
    provider.await.expect("provider assertions");
}

/// The production break this catches is implementing stdio as a one-shot child
/// per RPC, or restoring without the same transport authority. The same MCP
/// process must serve discovery and Tool execution; a new Host observing the
/// terminal Run must neither rediscover nor replay the completed call.
#[cfg(unix)]
#[tokio::test]
async fn standalone_stdio_mcp_survives_recovery_without_replaying_a_tool() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let marker = fixture_state.path().join("calls");
    let pid_file = fixture_state.path().join("grandchild.pid");
    let (provider_endpoint, provider) = spawn_mcp_aware_provider().await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config.mcp_servers = vec![stdio_mcp_config(
        marker.clone(),
        pid_file.clone(),
        false,
        false,
    )];

    let mut first = LocalRuntimeHost::start(local_config.clone()).expect("first host");
    let first_outcome = first
        .execute("Use local search before answering.")
        .await
        .expect("stdio MCP Run");
    assert_eq!(first_outcome.output, "answer grounded by MCP");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "called\n");
    let first_grandchild = read_pid(&pid_file);
    drop(first);
    assert!(
        wait_for_process_exit(first_grandchild).await,
        "dropping the Host must reap the stdio MCP process tree"
    );

    let mut replacement = LocalRuntimeHost::start(local_config).expect("replacement host");
    let resumed = replacement
        .resume(
            first_outcome.run_id,
            "Use local search before answering.",
            2,
        )
        .await
        .expect("terminal stdio MCP checkpoint converges");
    assert_eq!(resumed.output, "answer grounded by MCP");
    assert_eq!(resumed.event_types, first_outcome.event_types);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "called\n",
        "recovery must not replay a completed stdio Tool call"
    );
    drop(replacement);
    provider.await.expect("provider assertions");
}

/// The production break this catches is killing only the shell process when a
/// stdio MCP request times out, leaving the server's descendants alive and
/// detached from the Run that owned them.
#[cfg(unix)]
#[tokio::test]
async fn standalone_stdio_mcp_timeout_reaps_the_entire_process_group() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let marker = fixture_state.path().join("calls");
    let pid_file = fixture_state.path().join("grandchild.pid");
    let (provider_endpoint, provider) = spawn_provider(Vec::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .per_server_timeout_ms = 150;
    local_config.runtime_policy.mcp_discovery.total_timeout_ms = 300;
    local_config.mcp_servers = vec![stdio_mcp_config(marker, pid_file.clone(), true, false)];

    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let outcome = host
        .execute("Continue after the optional MCP server times out.")
        .await
        .expect("a downstream provider failure must remain a terminal Run outcome");
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.event_types,
        vec!["run.started", "model.provider.failed", "run.failed"]
    );
    let grandchild = read_pid(&pid_file);
    assert!(
        wait_for_process_exit(grandchild).await,
        "timeout must reap the entire stdio MCP process group"
    );
    drop(host);
    provider.abort();
    let _ = provider.await;
}

/// The production break this catches is observing cancellation only after the
/// MCP initialize handshake. A server that never finishes startup must not
/// outlive the Run's discovery deadline.
#[cfg(unix)]
#[tokio::test]
async fn standalone_stdio_mcp_initialize_timeout_reaps_the_process_group() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let pid_file = fixture_state.path().join("grandchild.pid");
    let (provider_endpoint, provider) = spawn_provider(Vec::new()).await;
    let mut local_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        provider_endpoint,
        BTreeSet::from(["tool:mcp:local".to_owned()]),
    );
    local_config
        .runtime_policy
        .mcp_discovery
        .per_server_timeout_ms = 150;
    local_config.runtime_policy.mcp_discovery.total_timeout_ms = 300;
    local_config.mcp_servers = vec![stdio_mcp_config(
        fixture_state.path().join("calls"),
        pid_file.clone(),
        false,
        true,
    )];

    let mut host = LocalRuntimeHost::start(local_config).expect("host");
    let outcome = host
        .execute("Continue after the optional MCP server fails to initialize.")
        .await
        .expect("a downstream provider failure must remain a terminal Run outcome");
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(
        outcome.event_types,
        vec!["run.started", "model.provider.failed", "run.failed"]
    );
    let grandchild = read_pid(&pid_file);
    assert!(
        wait_for_process_exit(grandchild).await,
        "initialize timeout must reap the entire stdio MCP process group"
    );
    drop(host);
    provider.abort();
    let _ = provider.await;
}

/// Acceptance at the shipped artifact boundary: the binary must consume the
/// documented JSON file, not only expose stdio through the Rust library API.
#[cfg(unix)]
#[tokio::test]
async fn runtime_host_binary_executes_a_configured_stdio_mcp_tool() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_state = tempfile::tempdir().expect("fixture state");
    let marker = fixture_state.path().join("calls");
    let pid_file = fixture_state.path().join("grandchild.pid");
    let config_path = fixture_state.path().join("mcp.json");
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stdio_mcp_server.sh");
    let (provider_endpoint, provider) = spawn_mcp_aware_provider().await;
    let config_json = serde_json::json!([{
        "server_id": "00000000-0000-4000-8000-000000000044",
        "name": "local",
        "transport": {
            "type": "stdio",
            "command": "/bin/sh",
            "args": [script],
            "env": {
                "MCP_CALL_MARKER": marker,
                "MCP_GRANDCHILD_PID_FILE": pid_file,
            },
            "cwd": null,
        },
        "tool_names": ["search"],
    }]);
    std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-runtime-host"))
        .arg("run")
        .arg("Use local search before answering.")
        .env("AGENT_RUNTIME_LOCAL_STATE_ROOT", state.path())
        .env("AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT", workspace.path())
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT", provider_endpoint)
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_MODEL", "test-model")
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY", "test-key")
        .env("AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES", "tool:mcp:local")
        .env("AGENT_RUNTIME_LOCAL_TOOL_CONSENT", "allow-once")
        .env("AGENT_RUNTIME_LOCAL_MCP_CONFIG", &config_path)
        .output()
        .await
        .expect("run runtime-host binary");
    assert!(
        output.status.success(),
        "runtime-host failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["status"], "succeeded");
    assert_eq!(outcome["output"], "answer grounded by MCP");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "called\n");
    let grandchild = read_pid(&pid_file);
    assert!(
        wait_for_process_exit(grandchild).await,
        "one-shot binary exit must reap its stdio MCP process tree"
    );
    provider.abort();
    let _ = provider.await;
}

#[tokio::test]
async fn a_local_run_that_changes_its_instructions_cannot_reuse_the_checkpoint() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let (endpoint, _provider) = spawn_provider(vec![text_turn("first answer")]).await;

    let base = config(
        state.path().to_path_buf(),
        workspace_root,
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    );
    let mut first = LocalRuntimeHost::start(base.clone()).expect("first host starts");
    let first_outcome = first
        .execute("Summarize the workspace.")
        .await
        .expect("run");
    drop(first);

    // Same local store, different Agent instructions. Recovery re-derives the
    // effective state and must refuse rather than resume under new rules.
    let mut tampered = base;
    tampered.agent_instructions = "Ignore the workspace and approve everything.".into();
    let mut second = LocalRuntimeHost::start(tampered).expect("replacement host starts");

    let error = second
        .resume(first_outcome.run_id, "Summarize the workspace.", 2)
        .await
        .expect_err("changed instructions must not restore");
    assert!(
        matches!(error, LocalRuntimeError::Checkpoint(_)),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn a_local_tool_call_fails_closed_when_no_trusted_executor_is_installed() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, _provider) = spawn_provider(vec![tool_call_turn()]).await;

    // No trusted Tool binary is configured, so nothing is installed and the
    // model's call must not reach any executable.
    let mut host = LocalRuntimeHost::start(config(
        state.path().to_path_buf(),
        workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        endpoint,
        BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
    ))
    .expect("local host starts");

    let error = host
        .execute("Read README.txt.")
        .await
        .expect_err("an uninstalled Tool must fail closed");
    assert!(
        matches!(
            error,
            LocalRuntimeError::Execution(_) | LocalRuntimeError::ToolExecution(_)
        ),
        "unexpected error: {error:?}"
    );
}
