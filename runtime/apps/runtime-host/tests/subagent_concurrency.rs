//! Bounded parallel subagent execution in the standalone Rust Host.
//!
//! The HTTP model transport, parent and child Agent loops, filesystem events
//! and Checkpoints are real. The model peer is deterministic so the test can
//! hold the first child response until it observes the second child request.

use agent_protocol::{RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, SubagentRole};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::retention::RuntimeRetentionPolicy;
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunState, LocalRuntimeConfig,
    LocalRuntimeHost, LocalToolConsent, WORKSPACE_READ_SCOPE, local_invocation_context,
};
use agent_runtime_worker::WorkerProcessor;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn metered_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":100,\"completion_tokens\":50}}}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn parallel_spawn_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [
                {
                    "index": 0,
                    "id": "call_alpha",
                    "type": "function",
                    "function": {
                        "name": "agent.spawn",
                        "arguments": serde_json::json!({
                            "role": "worker",
                            "input": "Solve alpha independently.",
                            "max_tokens": 400,
                            "max_cost_cents": 30,
                            "max_duration_seconds": 20
                        }).to_string()
                    }
                },
                {
                    "index": 1,
                    "id": "call_beta",
                    "type": "function",
                    "function": {
                        "name": "agent.spawn",
                        "arguments": serde_json::json!({
                            "role": "worker",
                            "input": "Solve beta independently.",
                            "max_tokens": 400,
                            "max_cost_cents": 30,
                            "max_duration_seconds": 20
                        }).to_string()
                    }
                }
            ]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn content_filter_turn() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n\
     data: [DONE]\n\n"
        .into()
}

fn parallel_approval_spawn_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [
                {
                    "index": 0,
                    "id": "call_approval_alpha",
                    "type": "function",
                    "function": {
                        "name": "agent.spawn",
                        "arguments": serde_json::json!({
                            "role": "worker",
                            "input": "Alpha must read evidence.",
                            "max_tokens": 400,
                            "max_cost_cents": 30,
                            "max_duration_seconds": 20
                        }).to_string()
                    }
                },
                {
                    "index": 1,
                    "id": "call_fast_beta",
                    "type": "function",
                    "function": {
                        "name": "agent.spawn",
                        "arguments": serde_json::json!({
                            "role": "worker",
                            "input": "Beta completes without tools.",
                            "max_tokens": 400,
                            "max_cost_cents": 30,
                            "max_duration_seconds": 20
                        }).to_string()
                    }
                }
            ]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn workspace_read_turn() -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_alpha_read",
                "type": "function",
                "function": {
                    "name": "workspace.read_text",
                    "arguments": serde_json::json!({"path": "EVIDENCE.txt"}).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn narrated_workspace_read_turn() -> String {
    let text_delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"content": "I will inspect the durable evidence before answering. "}
        }]
    });
    let tool_delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_history_read",
                "type": "function",
                "function": {
                    "name": "workspace.read_text",
                    "arguments": serde_json::json!({"path": "EVIDENCE.txt"}).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {text_delta}\n\ndata: {tool_delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn trusted_tool_binary() -> Option<std::path::PathBuf> {
    let mut current = std::env::current_exe().ok()?;
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = vec![0u8; 128 * 1024];
    let read = socket.read(&mut bytes).await.expect("read model request");
    String::from_utf8_lossy(&bytes[..read]).into_owned()
}

fn model_request_payload(request: &str) -> serde_json::Value {
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("model request has an HTTP body");
    serde_json::from_str(body).expect("model request body is JSON")
}

fn model_messages(request: &str) -> Vec<(String, String)> {
    model_request_payload(request)["messages"]
        .as_array()
        .expect("model request carries messages")
        .iter()
        .map(|message| {
            let role = message["role"].as_str().expect("model message has a role");
            (
                role.to_owned(),
                message["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn tool_result_content(request: &str, tool_call_id: &str) -> serde_json::Value {
    let payload = model_request_payload(request);
    let message = payload["messages"]
        .as_array()
        .expect("model request carries messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == tool_call_id)
        .expect("model request carries the expected Tool result");
    serde_json::from_str(
        message["content"]
            .as_str()
            .expect("Tool result content is encoded JSON"),
    )
    .expect("Tool result content decodes")
}

fn conversation_messages(request: &str) -> Vec<(String, String)> {
    model_messages(request)
        .into_iter()
        .filter(|(role, _)| matches!(role.as_str(), "user" | "assistant"))
        .collect()
}

async fn respond(socket: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write model response");
    socket.flush().await.expect("flush model response");
}

async fn spawn_parallel_provider() -> (String, tokio::task::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let parent_request = read_request(&mut parent).await;
        assert!(parent_request.contains("Delegate alpha and beta."));
        assert!(parent_request.contains("agent.spawn"));
        respond(&mut parent, &parallel_spawn_turn()).await;

        let (mut first_child, _) = listener.accept().await.expect("first child turn");
        let first_request = read_request(&mut first_child).await;
        let second = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        let observed_parallel = second.is_ok();
        let (mut second_child, _) = match second {
            Ok(accepted) => accepted.expect("second concurrent child turn"),
            Err(_) => {
                let first_text = if first_request.contains("Solve alpha independently.") {
                    "alpha solved"
                } else {
                    "beta solved"
                };
                respond(&mut first_child, &metered_text_turn(first_text)).await;
                listener.accept().await.expect("second serial child turn")
            }
        };
        let second_request = read_request(&mut second_child).await;
        assert_ne!(
            first_request.contains("Solve alpha independently."),
            second_request.contains("Solve alpha independently."),
            "the provider must receive one alpha and one beta child"
        );
        let second_text = if second_request.contains("Solve alpha independently.") {
            "alpha solved"
        } else {
            "beta solved"
        };
        respond(&mut second_child, &metered_text_turn(second_text)).await;
        if observed_parallel {
            let first_text = if first_request.contains("Solve alpha independently.") {
                "alpha solved"
            } else {
                "beta solved"
            };
            // Complete in reverse acceptance order so the parent cannot rely
            // on child completion order for Tool result binding.
            respond(&mut first_child, &metered_text_turn(first_text)).await;
        }

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        for expected in ["call_alpha", "call_beta", "alpha solved", "beta solved"] {
            assert!(
                final_request.contains(expected),
                "parent final turn did not contain {expected}: {final_request}"
            );
        }
        respond(
            &mut parent_final,
            &text_turn("parent combined both results"),
        )
        .await;
        observed_parallel
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_parallel_blocking_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &parallel_spawn_turn()).await;

        let (mut first_child, _) = listener.accept().await.expect("first child");
        let first_request = read_request(&mut first_child).await;
        let (mut second_child, _) = listener.accept().await.expect("second child");
        let second_request = read_request(&mut second_child).await;
        assert_ne!(
            first_request.contains("Solve alpha independently."),
            second_request.contains("Solve alpha independently.")
        );
        first_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start first stream");
        first_child.flush().await.expect("flush first stream");
        second_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start second stream");
        second_child.flush().await.expect("flush second stream");
        let _ = started_tx.send(());
        let first = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if first_child.write_all(b": first-running\n\n").await.is_err()
                    || first_child.flush().await.is_err()
                {
                    break;
                }
            }
        };
        let second = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if second_child
                    .write_all(b": second-running\n\n")
                    .await
                    .is_err()
                    || second_child.flush().await.is_err()
                {
                    break;
                }
            }
        };
        tokio::join!(first, second);
        let _ = closed_tx.send(());
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        started_rx,
        closed_rx,
        provider,
    )
}

fn single_spawn_turn(max_duration_seconds: u64) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_deadline_child",
                "type": "function",
                "function": {
                    "name": "agent.spawn",
                    "arguments": serde_json::json!({
                        "role": "worker",
                        "input": "Keep working until the shared deadline stops you.",
                        "max_tokens": 400,
                        "max_cost_cents": 30,
                        "max_duration_seconds": max_duration_seconds
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn async_spawn_turn(max_duration_seconds: u64) -> String {
    async_spawn_named_turn(
        "call_async_child",
        "Keep working until the shared deadline stops you.",
        max_duration_seconds,
    )
}

fn async_spawn_named_turn(call_id: &str, input: &str, max_duration_seconds: u64) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.spawn",
                    "arguments": serde_json::json!({
                        "role": "worker",
                        "input": input,
                        "max_tokens": 400,
                        "max_cost_cents": 30,
                        "max_duration_seconds": max_duration_seconds,
                        "mode": "async"
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn parallel_send_and_wait_turn(first_agent_id: Uuid, second_agent_id: Uuid) -> String {
    let calls = [
        serde_json::json!({
            "index": 0,
            "id": "call_ledger_send_a",
            "type": "function",
            "function": {
                "name": "agent.send",
                "arguments": serde_json::json!({
                    "agent_id": first_agent_id,
                    "generation": 1,
                    "message": "Run ledger task A.",
                    "idempotency_key": "ledger-host-a",
                    "interrupt": false
                }).to_string()
            }
        }),
        serde_json::json!({
            "index": 1,
            "id": "call_ledger_send_b",
            "type": "function",
            "function": {
                "name": "agent.send",
                "arguments": serde_json::json!({
                    "agent_id": second_agent_id,
                    "generation": 1,
                    "message": "Run ledger task B.",
                    "idempotency_key": "ledger-host-b",
                    "interrupt": false
                }).to_string()
            }
        }),
        serde_json::json!({
            "index": 2,
            "id": "call_ledger_wait_a",
            "type": "function",
            "function": {
                "name": "agent.wait",
                "arguments": serde_json::json!({
                    "agent_id": first_agent_id,
                    "timeout_ms": 2_000
                }).to_string()
            }
        }),
        serde_json::json!({
            "index": 3,
            "id": "call_ledger_wait_b",
            "type": "function",
            "function": {
                "name": "agent.wait",
                "arguments": serde_json::json!({
                    "agent_id": second_agent_id,
                    "timeout_ms": 2_000
                }).to_string()
            }
        }),
    ];
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": calls}}]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn wait_turn(call_id: &str, agent_id: Uuid, timeout_ms: u64) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.wait",
                    "arguments": serde_json::json!({
                        "agent_id": agent_id,
                        "timeout_ms": timeout_ms
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn history_turn(call_id: &str, agent_id: Uuid) -> String {
    history_turn_at_generation(call_id, agent_id, None)
}

fn history_turn_at_generation(call_id: &str, agent_id: Uuid, generation: Option<u64>) -> String {
    let mut arguments = serde_json::json!({
        "agent_id": agent_id,
        "limit": 20
    });
    if let Some(generation) = generation {
        arguments["generation"] = serde_json::json!(generation);
    }
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.history",
                    "arguments": arguments.to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn rollback_turn(
    call_id: &str,
    agent_id: Uuid,
    generation: u64,
    through_activation_ordinal: u64,
) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.rollback",
                    "arguments": serde_json::json!({
                        "agent_id": agent_id,
                        "generation": generation,
                        "through_activation_ordinal": through_activation_ordinal
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn fork_turn(call_id: &str, source_agent_id: Uuid, through_activation_ordinal: u64) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.fork",
                    "arguments": serde_json::json!({
                        "source_agent_id": source_agent_id,
                        "source_generation": 1,
                        "through_activation_ordinal": through_activation_ordinal,
                        "max_tokens": 200,
                        "max_cost_cents": 8,
                        "max_duration_seconds": 15
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn close_turn(call_id: &str, agent_id: Uuid) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.close",
                    "arguments": serde_json::json!({"agent_id": agent_id}).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn send_turn(call_id: &str, agent_id: Uuid, message: &str) -> String {
    send_turn_with_optional_generation(
        call_id,
        agent_id,
        Some(1),
        message,
        &format!("subagent-message:{call_id}"),
    )
}

fn send_turn_at_generation(
    call_id: &str,
    agent_id: Uuid,
    generation: u64,
    message: &str,
) -> String {
    send_turn_with_optional_generation(
        call_id,
        agent_id,
        Some(generation),
        message,
        &format!("subagent-message:{call_id}"),
    )
}

fn send_turn_with_key(
    call_id: &str,
    agent_id: Uuid,
    message: &str,
    idempotency_key: &str,
) -> String {
    send_turn_with_optional_generation(call_id, agent_id, Some(1), message, idempotency_key)
}

fn send_turn_with_optional_generation(
    call_id: &str,
    agent_id: Uuid,
    generation: Option<u64>,
    message: &str,
    idempotency_key: &str,
) -> String {
    let mut arguments = serde_json::json!({
        "agent_id": agent_id,
        "message": message,
        "idempotency_key": idempotency_key
    });
    if let Some(generation) = generation {
        arguments["generation"] = serde_json::json!(generation);
    }
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.send",
                    "arguments": arguments.to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn interrupt_send_turn(
    call_id: &str,
    agent_id: Uuid,
    message: &str,
    idempotency_key: &str,
) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": "agent.send",
                    "arguments": serde_json::json!({
                        "agent_id": agent_id,
                        "generation": 1,
                        "message": message,
                        "idempotency_key": idempotency_key,
                        "interrupt": true
                    }).to_string()
                }
            }]}
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn first_uuid(text: &str) -> Option<Uuid> {
    text.as_bytes().windows(36).find_map(|window| {
        std::str::from_utf8(window)
            .ok()
            .and_then(|candidate| Uuid::parse_str(candidate).ok())
    })
}

fn is_parent_after_async_spawn(request: &str) -> bool {
    request.contains("call_async_child") && request.contains("agent_id")
}

async fn spawn_async_handle_provider() -> (String, tokio::task::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let parent_request = read_request(&mut parent).await;
        assert!(parent_request.contains("agent.spawn"));
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let second = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        let (mut second, _) = match second {
            Ok(accepted) => accepted.expect("second asynchronous post-spawn turn"),
            Err(_) => {
                // Let the old synchronous implementation finish cleanly. The
                // boolean assertion below is the behavioural RED: spawn did
                // not return a handle while the child was still active.
                respond(
                    &mut first,
                    &metered_text_turn("child finished synchronously"),
                )
                .await;
                let (mut resumed, _) = listener.accept().await.expect("parent after child result");
                let _ = read_request(&mut resumed).await;
                respond(&mut resumed, &text_turn("legacy synchronous completion")).await;
                return false;
            }
        };
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child, child_request) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second, second_request)
            } else {
                (second, second_request, first, first_request)
            };
        assert!(
            spawn_result.contains("agent_id"),
            "spawn result did not expose a stable agent_id: {spawn_result}"
        );
        assert!(child_request.contains("Solve only the assigned independent task."));
        assert!(child_request.contains("Keep working until the shared deadline stops you."));
        let agent_id = Uuid::parse_str(
            tool_result_content(&spawn_result, "call_async_child")["agent_id"]
                .as_str()
                .expect("spawn result contains agent id"),
        )
        .expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_short", agent_id, 50),
        )
        .await;

        let (mut parent_after_timeout, _) =
            listener.accept().await.expect("parent after wait timeout");
        let timed_out_result = read_request(&mut parent_after_timeout).await;
        assert!(timed_out_result.contains("timed_out"));
        assert!(timed_out_result.contains("true"));
        respond(
            &mut parent_after_timeout,
            &wait_turn("call_wait_terminal", agent_id, 2_000),
        )
        .await;

        respond(
            &mut child,
            &metered_text_turn("child finished asynchronously"),
        )
        .await;

        let (mut parent_final, _) = listener
            .accept()
            .await
            .expect("parent after child terminal");
        let terminal_result = read_request(&mut parent_final).await;
        assert!(terminal_result.contains("child finished asynchronously"));
        assert!(terminal_result.contains("succeeded"));
        respond(&mut parent_final, &text_turn("parent observed async child")).await;
        true
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_close_provider() -> (String, tokio::task::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .expect("spawn did not return before child completion")
            .expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &close_turn("call_close_child", agent_id),
        )
        .await;

        let child_closed = tokio::time::timeout(Duration::from_secs(1), async {
            let mut byte = [0_u8; 1];
            loop {
                match child.read(&mut byte).await {
                    Ok(0) | Err(_) => break true,
                    Ok(_) => continue,
                }
            }
        });
        let parent_result = tokio::time::timeout(Duration::from_secs(1), listener.accept());
        let (closed, parent_result) = tokio::join!(child_closed, parent_result);
        let Ok(true) = closed else {
            return false;
        };
        let Ok(Ok((mut parent_after_close, _))) = parent_result else {
            return false;
        };
        let close_result = read_request(&mut parent_after_close).await;
        assert!(close_result.contains("cancelled"));
        respond(
            &mut parent_after_close,
            &text_turn("parent closed async child"),
        )
        .await;
        true
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_recovery_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
    oneshot::Receiver<()>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<Uuid>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (connections_established_tx, connections_established_rx) = oneshot::channel();
    let (crash_started_tx, crash_started_rx) = oneshot::channel();
    let (crash_observed_tx, crash_observed_rx) = oneshot::channel();
    let (connections_closed_tx, connections_closed_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut stranded_parent, spawn_result, mut stranded_child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = Uuid::parse_str(
            tool_result_content(&spawn_result, "call_async_child")["agent_id"]
                .as_str()
                .expect("spawn result contains agent id"),
        )
        .expect("spawn result contains agent UUID");
        connections_established_tx
            .send(())
            .expect("crash driver must still be waiting for both Provider connections");

        let parent_closed = async {
            let mut byte = [0_u8; 1];
            loop {
                match stranded_parent.read(&mut byte).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        };
        let child_closed = async {
            let mut byte = [0_u8; 1];
            loop {
                match stranded_child.read(&mut byte).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        };
        crash_started_rx
            .await
            .expect("test must signal the simulated crash");
        crash_observed_tx
            .send(())
            .expect("crash observer must still be waiting");
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(parent_closed, child_closed);
        })
        .await
        .expect("crashed runtime left provider sockets open");
        connections_closed_tx
            .send(())
            .expect("crash observer must still be waiting");

        let (mut first_recovered, _) = listener.accept().await.expect("first recovered turn");
        let first_recovered_request = read_request(&mut first_recovered).await;
        let (mut second_recovered, _) = listener.accept().await.expect("second recovered turn");
        let second_recovered_request = read_request(&mut second_recovered).await;
        let (mut recovered_parent, mut recovered_child, recovered_child_request) =
            if first_recovered_request.contains(&agent_id.to_string()) {
                (first_recovered, second_recovered, second_recovered_request)
            } else {
                assert!(second_recovered_request.contains(&agent_id.to_string()));
                (second_recovered, first_recovered, first_recovered_request)
            };
        assert!(
            recovered_child_request.contains("Keep working until the shared deadline stops you.")
        );
        respond(
            &mut recovered_parent,
            &wait_turn("call_wait_recovered", agent_id, 2_000),
        )
        .await;
        respond(
            &mut recovered_child,
            &metered_text_turn("child recovered from durable handle"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("recovered parent final");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("child recovered from durable handle"));
        respond(
            &mut parent_final,
            &text_turn("parent recovered async handle"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        connections_established_rx,
        crash_started_tx,
        crash_observed_rx,
        connections_closed_rx,
        provider,
    )
}

async fn spawn_async_send_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_first", agent_id, 2_000),
        )
        .await;
        respond(&mut child, &metered_text_turn("first child turn complete")).await;

        let (mut parent_after_first, _) = listener.accept().await.expect("parent after first turn");
        let first_result = read_request(&mut parent_after_first).await;
        assert!(first_result.contains("first child turn complete"));
        respond(
            &mut parent_after_first,
            &send_turn(
                "call_send_followup",
                agent_id,
                "Check the follow-up evidence.",
            ),
        )
        .await;

        let (mut first_followup, _) = listener.accept().await.expect("first follow-up connection");
        let first_followup_request = read_request(&mut first_followup).await;
        let (mut second_followup, _) = listener
            .accept()
            .await
            .expect("second follow-up connection");
        let second_followup_request = read_request(&mut second_followup).await;
        let (mut parent_after_send, send_result, mut followup_child, followup_request) =
            if first_followup_request.contains("submission_id") {
                (
                    first_followup,
                    first_followup_request,
                    second_followup,
                    second_followup_request,
                )
            } else {
                (
                    second_followup,
                    second_followup_request,
                    first_followup,
                    first_followup_request,
                )
            };
        assert!(send_result.contains(&agent_id.to_string()));
        assert!(followup_request.contains("Check the follow-up evidence."));
        assert_eq!(
            conversation_messages(&followup_request),
            vec![
                (
                    "user".into(),
                    "Keep working until the shared deadline stops you.".into(),
                ),
                ("assistant".into(), "first child turn complete".into()),
                ("user".into(), "Check the follow-up evidence.".into()),
            ],
            "a persistent handle must continue the child conversation as model roles, not restart it or flatten history into instructions"
        );
        assert!(
            model_messages(&followup_request)
                .iter()
                .filter(|(role, _)| role == "system")
                .all(|(_, content)| {
                    !content.contains("Keep working until the shared deadline stops you.")
                        && !content.contains("first child turn complete")
                }),
            "conversation history must not be promoted into system instructions"
        );
        assert!(send_result.contains(&format!("{agent_id}:1")));
        respond(
            &mut parent_after_send,
            &send_turn_with_key(
                "call_send_duplicate",
                agent_id,
                "Check the follow-up evidence.",
                "subagent-message:call_send_followup",
            ),
        )
        .await;

        let (mut parent_after_duplicate, _) = listener
            .accept()
            .await
            .expect("parent after duplicate send");
        let duplicate_result = read_request(&mut parent_after_duplicate).await;
        assert!(
            duplicate_result.contains(&format!("{agent_id}:1")),
            "duplicate idempotency key did not replay the original submission receipt"
        );
        respond(
            &mut parent_after_duplicate,
            &wait_turn("call_wait_followup", agent_id, 2_000),
        )
        .await;
        respond(
            &mut followup_child,
            &metered_text_turn("follow-up child turn complete"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("follow-up child turn complete"));
        assert!(final_request.contains(&agent_id.to_string()));
        respond(&mut parent_final, &history_turn("call_history", agent_id)).await;

        let (mut parent_after_history, _) =
            listener.accept().await.expect("parent after history query");
        let history_result = read_request(&mut parent_after_history).await;
        let history = tool_result_content(&history_result, "call_history");
        assert_eq!(history["turns"].as_array().map(Vec::len), Some(2));
        assert_eq!(history["turns"][0]["activation_ordinal"], 0);
        assert_eq!(
            history["turns"][0]["input"],
            "Keep working until the shared deadline stops you."
        );
        assert_eq!(
            history["turns"][0]["result"]["content"]["text"],
            "first child turn complete"
        );
        assert_eq!(history["turns"][1]["activation_ordinal"], 1);
        assert_eq!(
            history["turns"][1]["input"],
            "Check the follow-up evidence."
        );
        assert_eq!(
            history["turns"][1]["result"]["content"]["text"],
            "follow-up child turn complete"
        );
        respond(
            &mut parent_after_history,
            &text_turn("parent completed persistent dialog"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_tree_budget_provider() -> (String, tokio::task::JoinHandle<Vec<u64>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first spawn");
        let _ = read_request(&mut parent).await;
        respond(
            &mut parent,
            &async_spawn_named_turn("call_ledger_spawn_a", "Create ledger handle A.", 20),
        )
        .await;

        let (mut first, _) = listener.accept().await.expect("first A connection");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second A connection");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_a, parent_a_request, mut child_a) = if first_request
            .contains("call_ledger_spawn_a")
            && first_request.contains("agent_id")
        {
            (first, first_request, second)
        } else {
            (second, second_request, first)
        };
        let first_agent_id = Uuid::parse_str(
            tool_result_content(&parent_a_request, "call_ledger_spawn_a")["agent_id"]
                .as_str()
                .expect("first handle id"),
        )
        .expect("first handle UUID");
        respond(
            &mut parent_after_a,
            &wait_turn("call_ledger_initial_wait_a", first_agent_id, 2_000),
        )
        .await;
        respond(&mut child_a, &text_turn("ledger handle A ready")).await;

        let (mut parent_second_spawn, _) = listener.accept().await.expect("parent second spawn");
        let second_spawn_request = read_request(&mut parent_second_spawn).await;
        assert!(second_spawn_request.contains("ledger handle A ready"));
        respond(
            &mut parent_second_spawn,
            &async_spawn_named_turn("call_ledger_spawn_b", "Create ledger handle B.", 20),
        )
        .await;

        let (mut first, _) = listener.accept().await.expect("first B connection");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second B connection");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_b, parent_b_request, mut child_b) = if first_request
            .contains("call_ledger_spawn_b")
            && first_request.contains("agent_id")
        {
            (first, first_request, second)
        } else {
            (second, second_request, first)
        };
        let second_agent_id = Uuid::parse_str(
            tool_result_content(&parent_b_request, "call_ledger_spawn_b")["agent_id"]
                .as_str()
                .expect("second handle id"),
        )
        .expect("second handle UUID");
        assert_ne!(first_agent_id, second_agent_id);
        respond(
            &mut parent_after_b,
            &wait_turn("call_ledger_initial_wait_b", second_agent_id, 2_000),
        )
        .await;
        respond(&mut child_b, &text_turn("ledger handle B ready")).await;

        let (mut parent_parallel_send, _) = listener.accept().await.expect("parent parallel send");
        let parallel_send_request = read_request(&mut parent_parallel_send).await;
        assert!(parallel_send_request.contains("ledger handle B ready"));
        respond(
            &mut parent_parallel_send,
            &parallel_send_and_wait_turn(first_agent_id, second_agent_id),
        )
        .await;

        let (mut first_child, _) = listener.accept().await.expect("first ledger child");
        let first_child_request = read_request(&mut first_child).await;
        let (mut second_child, _) = listener.accept().await.expect("second ledger child");
        let second_child_request = read_request(&mut second_child).await;
        assert!(
            first_child_request.contains("Run ledger task A.")
                || first_child_request.contains("Run ledger task B.")
        );
        assert!(
            second_child_request.contains("Run ledger task A.")
                || second_child_request.contains("Run ledger task B.")
        );
        let mut observed = vec![
            model_request_payload(&first_child_request)["max_tokens"]
                .as_u64()
                .expect("first child max_tokens"),
            model_request_payload(&second_child_request)["max_tokens"]
                .as_u64()
                .expect("second child max_tokens"),
        ];
        observed.sort_unstable();
        respond(&mut first_child, &text_turn("ledger task one complete")).await;
        respond(&mut second_child, &text_turn("ledger task two complete")).await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("ledger task one complete"));
        assert!(final_request.contains("ledger task two complete"));
        respond(&mut parent_final, &text_turn("tree budget verified")).await;
        observed
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_fork_provider() -> (String, tokio::task::JoinHandle<(Uuid, Uuid)>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut source_child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let source_agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        assert_eq!(
            tool_result_content(&spawn_result, "call_async_child")["generation"],
            1,
            "new persistent handles must expose the generation required by agent.fork"
        );
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_source", source_agent_id, 2_000),
        )
        .await;
        respond(&mut source_child, &narrated_workspace_read_turn()).await;

        let (mut source_after_tool, _) = listener
            .accept()
            .await
            .expect("source child after Tool result");
        let source_after_tool_request = read_request(&mut source_after_tool).await;
        assert_eq!(
            tool_result_content(&source_after_tool_request, "call_history_read")["text"],
            "fork source evidence"
        );
        respond(
            &mut source_after_tool,
            &metered_text_turn("source child turn complete"),
        )
        .await;

        let (mut parent_after_source, _) =
            listener.accept().await.expect("parent after source turn");
        let source_result = read_request(&mut parent_after_source).await;
        assert!(source_result.contains("source child turn complete"));
        respond(
            &mut parent_after_source,
            &fork_turn("call_fork", source_agent_id, 0),
        )
        .await;

        let (mut parent_after_fork, _) = listener.accept().await.expect("parent after fork");
        let fork_result_request = read_request(&mut parent_after_fork).await;
        let fork_result = tool_result_content(&fork_result_request, "call_fork");
        let forked_agent_id = fork_result["agent_id"]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .expect("fork result contains new agent UUID");
        assert_ne!(forked_agent_id, source_agent_id);
        assert_eq!(fork_result["generation"], 1);
        assert_eq!(fork_result["source_generation"], 1);
        assert_eq!(fork_result["through_activation_ordinal"], 0);
        assert_eq!(fork_result["budget"]["max_tokens"], 200);
        respond(
            &mut parent_after_fork,
            &send_turn(
                "call_send_fork",
                forked_agent_id,
                "Continue only on the fork.",
            ),
        )
        .await;

        let (mut first_fork, _) = listener.accept().await.expect("first fork continuation");
        let first_fork_request = read_request(&mut first_fork).await;
        let (mut second_fork, _) = listener.accept().await.expect("second fork continuation");
        let second_fork_request = read_request(&mut second_fork).await;
        let (mut parent_after_send, send_result, mut fork_child, fork_child_request) =
            if first_fork_request.contains("submission_id") {
                (
                    first_fork,
                    first_fork_request,
                    second_fork,
                    second_fork_request,
                )
            } else {
                (
                    second_fork,
                    second_fork_request,
                    first_fork,
                    first_fork_request,
                )
            };
        assert!(send_result.contains(&forked_agent_id.to_string()));
        assert_eq!(
            conversation_messages(&fork_child_request),
            vec![
                (
                    "user".into(),
                    "Keep working until the shared deadline stops you.".into(),
                ),
                (
                    "assistant".into(),
                    "I will inspect the durable evidence before answering. ".into(),
                ),
                ("assistant".into(), "source child turn complete".into()),
                ("user".into(), "Continue only on the fork.".into()),
            ],
            "the fork must inherit only the selected completed prefix"
        );
        let fork_messages = model_request_payload(&fork_child_request)["messages"]
            .as_array()
            .expect("fork request messages")
            .clone();
        assert_eq!(
            fork_messages
                .iter()
                .filter(|message| {
                    message["role"] == "assistant"
                        && message["tool_calls"].as_array().is_some_and(|calls| {
                            calls.iter().any(|call| call["id"] == "call_history_read")
                        })
                })
                .count(),
            1
        );
        assert_eq!(
            fork_messages
                .iter()
                .filter(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "call_history_read"
                })
                .count(),
            1,
            "fork context must preserve the completed Tool pair without scheduling it again"
        );
        respond(
            &mut parent_after_send,
            &wait_turn("call_wait_fork", forked_agent_id, 2_000),
        )
        .await;
        respond(
            &mut fork_child,
            &metered_text_turn("fork child turn complete"),
        )
        .await;

        let (mut parent_after_wait, _) = listener.accept().await.expect("parent after fork wait");
        let fork_terminal = read_request(&mut parent_after_wait).await;
        assert!(fork_terminal.contains("fork child turn complete"));
        respond(
            &mut parent_after_wait,
            &history_turn("call_source_history", source_agent_id),
        )
        .await;

        let (mut parent_after_source_history, _) = listener
            .accept()
            .await
            .expect("parent after source history");
        let source_history_request = read_request(&mut parent_after_source_history).await;
        let source_history = tool_result_content(&source_history_request, "call_source_history");
        assert_eq!(source_history["turns"].as_array().map(Vec::len), Some(1));
        assert_eq!(source_history["generation"], 1);
        assert!(source_history["forked_from"].is_null());
        respond(
            &mut parent_after_source_history,
            &history_turn("call_fork_history", forked_agent_id),
        )
        .await;

        let (mut parent_after_fork_history, _) =
            listener.accept().await.expect("parent after fork history");
        let fork_history_request = read_request(&mut parent_after_fork_history).await;
        let fork_history = tool_result_content(&fork_history_request, "call_fork_history");
        assert_eq!(fork_history["turns"].as_array().map(Vec::len), Some(2));
        assert_eq!(fork_history["turns"][0], source_history["turns"][0]);
        assert_eq!(fork_history["generation"], 1);
        assert_eq!(
            fork_history["forked_from"]["source_agent_id"],
            source_agent_id.to_string()
        );
        assert_eq!(fork_history["forked_from"]["through_activation_ordinal"], 0);
        respond(
            &mut parent_after_fork_history,
            &text_turn("parent completed fork lifecycle"),
        )
        .await;
        (source_agent_id, forked_agent_id)
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_rollback_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        assert_eq!(
            tool_result_content(&spawn_result, "call_async_child")["generation"],
            1
        );
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_rollback_source", agent_id, 2_000),
        )
        .await;
        respond(&mut child, &narrated_workspace_read_turn()).await;

        let (mut child_after_tool, _) = listener
            .accept()
            .await
            .expect("child after source Tool result");
        let child_after_tool_request = read_request(&mut child_after_tool).await;
        assert_eq!(
            tool_result_content(&child_after_tool_request, "call_history_read")["text"],
            "rollback source evidence"
        );
        respond(
            &mut child_after_tool,
            &metered_text_turn("generation one turn zero complete"),
        )
        .await;

        let (mut parent_after_first, _) = listener
            .accept()
            .await
            .expect("parent after first child turn");
        let first_result = read_request(&mut parent_after_first).await;
        assert!(first_result.contains("generation one turn zero complete"));
        respond(
            &mut parent_after_first,
            &send_turn(
                "call_send_generation_one",
                agent_id,
                "Complete generation one turn one.",
            ),
        )
        .await;

        let (mut first_followup, _) = listener.accept().await.expect("first follow-up");
        let first_followup_request = read_request(&mut first_followup).await;
        let (mut second_followup, _) = listener.accept().await.expect("second follow-up");
        let second_followup_request = read_request(&mut second_followup).await;
        let (mut parent_after_send, send_result, mut generation_one_child, child_request) =
            if first_followup_request.contains("submission_id") {
                (
                    first_followup,
                    first_followup_request,
                    second_followup,
                    second_followup_request,
                )
            } else {
                (
                    second_followup,
                    second_followup_request,
                    first_followup,
                    first_followup_request,
                )
            };
        assert!(send_result.contains(&agent_id.to_string()));
        assert!(child_request.contains("Complete generation one turn one."));
        respond(
            &mut parent_after_send,
            &wait_turn("call_wait_generation_one", agent_id, 2_000),
        )
        .await;
        respond(
            &mut generation_one_child,
            &metered_text_turn("generation one turn one complete"),
        )
        .await;

        let (mut parent_after_second, _) = listener
            .accept()
            .await
            .expect("parent after second child turn");
        let second_result = read_request(&mut parent_after_second).await;
        assert!(second_result.contains("generation one turn one complete"));
        respond(
            &mut parent_after_second,
            &rollback_turn("call_rollback", agent_id, 1, 0),
        )
        .await;

        let (mut parent_after_rollback, _) =
            listener.accept().await.expect("parent after rollback");
        let rollback_result_request = read_request(&mut parent_after_rollback).await;
        let rollback_result = tool_result_content(&rollback_result_request, "call_rollback");
        assert_eq!(rollback_result["agent_id"], agent_id.to_string());
        assert_eq!(rollback_result["from_generation"], 1);
        assert_eq!(rollback_result["generation"], 2);
        assert_eq!(rollback_result["through_activation_ordinal"], 0);
        respond(
            &mut parent_after_rollback,
            &history_turn_at_generation("call_history_generation_one", agent_id, Some(1)),
        )
        .await;

        let (mut parent_after_archive, _) = listener
            .accept()
            .await
            .expect("parent after archived history");
        let archived_request = read_request(&mut parent_after_archive).await;
        let archived = tool_result_content(&archived_request, "call_history_generation_one");
        assert_eq!(archived["generation"], 1);
        assert_eq!(archived["status"], "archived");
        assert_eq!(archived["turns"].as_array().map(Vec::len), Some(2));
        respond(
            &mut parent_after_archive,
            &history_turn("call_history_generation_two_head", agent_id),
        )
        .await;

        let (mut parent_after_head, _) = listener
            .accept()
            .await
            .expect("parent after current history");
        let current_head_request = read_request(&mut parent_after_head).await;
        let current_head =
            tool_result_content(&current_head_request, "call_history_generation_two_head");
        assert_eq!(current_head["generation"], 2);
        assert_eq!(current_head["turns"].as_array().map(Vec::len), Some(1));
        respond(
            &mut parent_after_head,
            &send_turn_at_generation(
                "call_send_generation_two",
                agent_id,
                2,
                "Continue only from the rolled-back head.",
            ),
        )
        .await;

        let (mut first_generation_two, _) = listener
            .accept()
            .await
            .expect("first generation two continuation");
        let first_generation_two_request = read_request(&mut first_generation_two).await;
        let (mut second_generation_two, _) = listener
            .accept()
            .await
            .expect("second generation two continuation");
        let second_generation_two_request = read_request(&mut second_generation_two).await;
        let (mut parent_after_send, send_result, mut generation_two_child, child_request) =
            if first_generation_two_request.contains("submission_id") {
                (
                    first_generation_two,
                    first_generation_two_request,
                    second_generation_two,
                    second_generation_two_request,
                )
            } else {
                (
                    second_generation_two,
                    second_generation_two_request,
                    first_generation_two,
                    first_generation_two_request,
                )
            };
        assert!(send_result.contains(&agent_id.to_string()));
        assert!(child_request.contains("Continue only from the rolled-back head."));
        assert!(child_request.contains("generation one turn zero complete"));
        assert!(
            !child_request.contains("generation one turn one complete"),
            "the superseded suffix leaked into the new generation model request"
        );
        let child_payload = model_request_payload(&child_request);
        let messages = child_payload["messages"]
            .as_array()
            .expect("generation two messages");
        assert_eq!(
            messages
                .iter()
                .filter(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "call_history_read"
                })
                .count(),
            1,
            "the retained Tool result must be context, not a pending execution"
        );
        respond(
            &mut parent_after_send,
            &wait_turn("call_wait_generation_two", agent_id, 2_000),
        )
        .await;
        respond(
            &mut generation_two_child,
            &metered_text_turn("generation two turn complete"),
        )
        .await;

        let (mut parent_after_generation_two, _) = listener
            .accept()
            .await
            .expect("parent after generation two turn");
        let generation_two_result = read_request(&mut parent_after_generation_two).await;
        assert!(generation_two_result.contains("generation two turn complete"));
        respond(
            &mut parent_after_generation_two,
            &history_turn("call_history_generation_two", agent_id),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        let final_history = tool_result_content(&final_request, "call_history_generation_two");
        assert_eq!(final_history["generation"], 2);
        assert_eq!(
            final_history["turns"].as_array().map(|turns| turns
                .iter()
                .map(|turn| turn["activation_ordinal"].as_u64().unwrap())
                .collect::<Vec<_>>()),
            Some(vec![0, 2])
        );
        respond(
            &mut parent_final,
            &text_turn("parent completed rollback lifecycle"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_tool_history_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_tool_turn", agent_id, 2_000),
        )
        .await;
        respond(&mut child, &narrated_workspace_read_turn()).await;

        let (mut child_after_tool, _) = listener.accept().await.expect("child after Tool result");
        let child_after_tool_request = read_request(&mut child_after_tool).await;
        let tool_result = tool_result_content(&child_after_tool_request, "call_history_read");
        assert_eq!(tool_result["text"], "durable child evidence");
        respond(
            &mut child_after_tool,
            &metered_text_turn("The durable evidence is confirmed."),
        )
        .await;

        let (mut parent_after_first, _) = listener.accept().await.expect("parent after first turn");
        let first_result = read_request(&mut parent_after_first).await;
        assert!(first_result.contains("The durable evidence is confirmed."));
        respond(
            &mut parent_after_first,
            &send_turn(
                "call_send_tool_followup",
                agent_id,
                "Explain which evidence you inspected.",
            ),
        )
        .await;

        let (mut first_followup, _) = listener.accept().await.expect("first follow-up connection");
        let first_followup_request = read_request(&mut first_followup).await;
        let (mut second_followup, _) = listener
            .accept()
            .await
            .expect("second follow-up connection");
        let second_followup_request = read_request(&mut second_followup).await;
        let (mut parent_after_send, send_result, mut followup_child, followup_request) =
            if first_followup_request.contains("submission_id") {
                (
                    first_followup,
                    first_followup_request,
                    second_followup,
                    second_followup_request,
                )
            } else {
                (
                    second_followup,
                    second_followup_request,
                    first_followup,
                    first_followup_request,
                )
            };
        assert!(send_result.contains(&agent_id.to_string()));

        let payload = model_request_payload(&followup_request);
        let messages = payload["messages"]
            .as_array()
            .expect("follow-up request carries messages");
        let prior_assistant = messages
            .iter()
            .find(|message| {
                message["role"] == "assistant"
                    && message["tool_calls"][0]["id"] == "call_history_read"
            })
            .expect("follow-up preserves the prior Assistant Tool call");
        assert_eq!(
            prior_assistant["content"],
            "I will inspect the durable evidence before answering. "
        );
        assert_eq!(
            prior_assistant["tool_calls"][0]["function"]["name"],
            "workspace.read_text"
        );
        let prior_tool_result = messages
            .iter()
            .find(|message| {
                message["role"] == "tool" && message["tool_call_id"] == "call_history_read"
            })
            .expect("follow-up preserves the prior Tool result");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                prior_tool_result["content"]
                    .as_str()
                    .expect("Tool result content is JSON")
            )
            .expect("Tool result content decodes")["text"],
            "durable child evidence"
        );
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant"
                && message["content"] == "The durable evidence is confirmed."
        }));
        assert_eq!(
            messages
                .last()
                .and_then(|message| message["content"].as_str()),
            Some("Explain which evidence you inspected.")
        );

        respond(
            &mut parent_after_send,
            &wait_turn("call_wait_tool_followup", agent_id, 2_000),
        )
        .await;
        respond(
            &mut followup_child,
            &metered_text_turn("I inspected EVIDENCE.txt through workspace.read_text."),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("I inspected EVIDENCE.txt"));
        respond(
            &mut parent_final,
            &text_turn("parent preserved the child Tool transcript"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_terminal_gap_recovery_provider() -> (
    String,
    oneshot::Receiver<Uuid>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Uuid>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (child_active_tx, child_active_rx) = oneshot::channel();
    let (release_child_tx, release_child_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_before_terminal_gap", agent_id, 10_000),
        )
        .await;
        child_active_tx
            .send(agent_id)
            .expect("test still waits for active child");
        release_child_rx
            .await
            .expect("test releases the child after preserving parent state");

        respond(&mut child, &narrated_workspace_read_turn()).await;
        let (mut child_after_tool, _) = listener.accept().await.expect("child after Tool result");
        let child_after_tool_request = read_request(&mut child_after_tool).await;
        assert_eq!(
            tool_result_content(&child_after_tool_request, "call_history_read")["text"],
            "durable child evidence"
        );
        respond(
            &mut child_after_tool,
            &metered_text_turn("The durable evidence is confirmed."),
        )
        .await;

        let (mut parent_first_final, _) = listener.accept().await.expect("parent first final turn");
        let parent_first_final_request = read_request(&mut parent_first_final).await;
        assert!(parent_first_final_request.contains("The durable evidence is confirmed."));
        respond(
            &mut parent_first_final,
            &text_turn("first parent process completed"),
        )
        .await;

        let (mut recovered_parent, _) = listener.accept().await.expect("recovered parent turn");
        let recovered_parent_request = read_request(&mut recovered_parent).await;
        assert!(recovered_parent_request.contains(&agent_id.to_string()));
        let mut recovered_parent =
            if recovered_parent_request.contains("The durable evidence is confirmed.") {
                recovered_parent
            } else {
                respond(
                    &mut recovered_parent,
                    &wait_turn("call_wait_recovered_terminal_gap", agent_id, 2_000),
                )
                .await;
                let (mut after_wait, _) = listener
                    .accept()
                    .await
                    .expect("recovered parent after child wait");
                let after_wait_request = read_request(&mut after_wait).await;
                assert!(after_wait_request.contains("The durable evidence is confirmed."));
                after_wait
            };
        respond(
            &mut recovered_parent,
            &send_turn(
                "call_send_after_terminal_gap",
                agent_id,
                "Explain which evidence you inspected after recovery.",
            ),
        )
        .await;

        let (mut first_followup, _) = listener.accept().await.expect("first recovered follow-up");
        let first_followup_request = read_request(&mut first_followup).await;
        let (mut second_followup, _) = listener.accept().await.expect("second recovered follow-up");
        let second_followup_request = read_request(&mut second_followup).await;
        let (mut parent_after_send, send_result, mut recovered_child, recovered_child_request) =
            if first_followup_request.contains("submission_id") {
                (
                    first_followup,
                    first_followup_request,
                    second_followup,
                    second_followup_request,
                )
            } else {
                (
                    second_followup,
                    second_followup_request,
                    first_followup,
                    first_followup_request,
                )
            };
        assert!(send_result.contains(&agent_id.to_string()));
        let messages = model_request_payload(&recovered_child_request)["messages"]
            .as_array()
            .expect("recovered child request carries messages")
            .clone();
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant"
                && message["tool_calls"][0]["id"] == "call_history_read"
                && message["content"] == "I will inspect the durable evidence before answering. "
        }));
        assert!(messages.iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "call_history_read"
        }));
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant"
                && message["content"] == "The durable evidence is confirmed."
        }));
        assert_eq!(
            messages
                .last()
                .and_then(|message| message["content"].as_str()),
            Some("Explain which evidence you inspected after recovery.")
        );

        respond(
            &mut parent_after_send,
            &wait_turn("call_wait_after_terminal_gap", agent_id, 2_000),
        )
        .await;
        respond(
            &mut recovered_child,
            &metered_text_turn("Recovered history identifies EVIDENCE.txt."),
        )
        .await;
        let (mut parent_final, _) = listener
            .accept()
            .await
            .expect("parent recovered final turn");
        let parent_final_request = read_request(&mut parent_final).await;
        assert!(parent_final_request.contains("Recovered history identifies EVIDENCE.txt."));
        respond(
            &mut parent_final,
            &text_turn("parent recovered the terminal Tool transcript"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        child_active_rx,
        release_child_tx,
        provider,
    )
}

async fn spawn_async_mailbox_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut initial_child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        initial_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start initial child stream");
        initial_child.flush().await.expect("flush initial child");
        respond(
            &mut parent_after_spawn,
            &send_turn_with_key(
                "call_queue_while_running",
                agent_id,
                "Run this after the active child.",
                "queued-followup-1",
            ),
        )
        .await;

        let (mut parent_after_queue, _) =
            listener.accept().await.expect("parent after queued send");
        let queued_result = read_request(&mut parent_after_queue).await;
        assert!(queued_result.contains("queued-followup-1"));
        assert!(queued_result.contains("queued"));
        respond(
            &mut parent_after_queue,
            &wait_turn("call_wait_active_then_promote", agent_id, 2_000),
        )
        .await;
        initial_child
            .write_all(
                &metered_text_turn("initial child completed before queued work").into_bytes(),
            )
            .await
            .expect("finish initial child");
        initial_child
            .flush()
            .await
            .expect("flush initial completion");

        let (mut first_promoted, _) = listener.accept().await.expect("first promoted connection");
        let first_promoted_request = read_request(&mut first_promoted).await;
        let (mut second_promoted, _) = listener.accept().await.expect("second promoted connection");
        let second_promoted_request = read_request(&mut second_promoted).await;
        let (mut parent_after_promotion, promotion_result, mut queued_child, queued_request) =
            if first_promoted_request.contains("active_message_sequence") {
                (
                    first_promoted,
                    first_promoted_request,
                    second_promoted,
                    second_promoted_request,
                )
            } else {
                (
                    second_promoted,
                    second_promoted_request,
                    first_promoted,
                    first_promoted_request,
                )
            };
        assert!(promotion_result.contains("initial child completed before queued work"));
        assert!(queued_request.contains("Run this after the active child."));
        respond(
            &mut parent_after_promotion,
            &wait_turn("call_wait_queued_child", agent_id, 2_000),
        )
        .await;
        respond(
            &mut queued_child,
            &metered_text_turn("queued child completed in FIFO order"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("queued child completed in FIFO order"));
        respond(
            &mut parent_final,
            &text_turn("parent drained persistent mailbox"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_interrupt_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut initial_child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        initial_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start initial child stream");
        initial_child.flush().await.expect("flush initial child");
        respond(
            &mut parent_after_spawn,
            &interrupt_send_turn(
                "call_interrupt_active",
                agent_id,
                "Redirect immediately to the replacement task.",
                "interrupt-active-1",
            ),
        )
        .await;

        let (mut first_after_interrupt, _) =
            tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .expect("interrupt did not launch the redirected turn")
                .expect("accept first post-interrupt request");
        let first_after_interrupt_request = read_request(&mut first_after_interrupt).await;
        let (mut second_after_interrupt, _) = listener
            .accept()
            .await
            .expect("accept second post-interrupt request");
        let second_after_interrupt_request = read_request(&mut second_after_interrupt).await;
        let (mut parent_after_interrupt, parent_result, mut redirected_child, redirected_request) =
            if first_after_interrupt_request.contains("submission_id") {
                (
                    first_after_interrupt,
                    first_after_interrupt_request,
                    second_after_interrupt,
                    second_after_interrupt_request,
                )
            } else {
                (
                    second_after_interrupt,
                    second_after_interrupt_request,
                    first_after_interrupt,
                    first_after_interrupt_request,
                )
            };
        assert!(parent_result.contains("interrupt-active-1"));
        assert!(parent_result.contains("accepted"));
        assert!(redirected_request.contains("Redirect immediately to the replacement task."));

        let mut byte = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(1), initial_child.read(&mut byte))
            .await
            .expect("interrupted child connection stayed open");
        assert!(matches!(closed, Ok(0) | Err(_)));

        respond(
            &mut parent_after_interrupt,
            &wait_turn("call_wait_redirected", agent_id, 2_000),
        )
        .await;
        respond(
            &mut redirected_child,
            &metered_text_turn("redirected child completed"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("redirected child completed"));
        respond(
            &mut parent_final,
            &text_turn("parent completed durable interrupt"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_async_interrupt_recovery_provider() -> (String, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut initial_child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        initial_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start initial child stream");
        initial_child.flush().await.expect("flush initial child");
        respond(
            &mut parent_after_spawn,
            &interrupt_send_turn(
                "call_interrupt_before_crash",
                agent_id,
                "Recover only this redirected task.",
                "interrupt-recovery-1",
            ),
        )
        .await;

        let mut recovered_parent = None;
        let mut redirected_child = None;
        for _ in 0..3 {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("replacement Host did not reconnect")
                .expect("accept replacement request");
            let request = read_request(&mut socket).await;
            if request.contains("submission_id") && request.contains("interrupt-recovery-1") {
                recovered_parent = Some(socket);
            } else if request.contains("Recover only this redirected task.") {
                redirected_child = Some(socket);
            } else {
                panic!("replacement relaunched the interrupted child input: {request}");
            }
            if recovered_parent.is_some() && redirected_child.is_some() {
                break;
            }
        }
        let mut recovered_parent = recovered_parent.expect("recovered parent model request");
        let mut redirected_child = redirected_child.expect("redirected child model request");
        respond(
            &mut recovered_parent,
            &wait_turn("call_wait_recovered_interrupt", agent_id, 2_000),
        )
        .await;
        respond(
            &mut redirected_child,
            &metered_text_turn("recovered redirect completed once"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("recovered redirect completed once"));
        respond(
            &mut parent_final,
            &text_turn("parent recovered durable interrupt"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn crash_after_interrupt_receipt(config: LocalRuntimeConfig, run_id: Uuid) -> Uuid {
    tokio::task::spawn_blocking(move || {
        let state_root = config.state_root.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("isolated runtime");
        runtime.block_on(async move {
            let mut host = LocalRuntimeHost::start(config).expect("first host");
            let execution = tokio::spawn(async move {
                host.execute_as(run_id, "Delegate the bounded child.").await
            });
            let accepted = tokio::time::timeout(Duration::from_secs(5), async {
                let mut after_sequence = 0;
                loop {
                    let events = LocalRuntimeHost::replay_events(
                        &state_root,
                        run_id,
                        after_sequence,
                    )
                    .expect("durable event stream");
                    for event in events {
                        after_sequence = event.sequence;
                        if event.event_type == "subagent.input.accepted"
                            && event.payload["interrupt"] == true
                        {
                            return event;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("interrupt receipt was not emitted before crash");
            let agent_id = accepted.payload["agent_id"]
                .as_str()
                .and_then(|value| Uuid::parse_str(value).ok())
                .expect("interrupt event agent id");
            let checkpoint_path = LocalRuntimeHost::checkpoint_path(&state_root, run_id);
            let checkpoint = LocalRuntimeHost::load_checkpoint(&checkpoint_path)
                .expect("interrupt receipt checkpoint");
            let checkpoint: serde_json::Value =
                serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
            assert_eq!(
                checkpoint["subagent_message_receipts"][agent_id.to_string()]
                    ["interrupt-recovery-1"]["status"],
                "queued",
                "test must crash before the redirect has been activated"
            );
            execution.abort();
            let _ = execution.await;
            agent_id
        })
    })
    .await
    .expect("crash runtime thread")
}

async fn spawn_async_send_recovery_provider()
-> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<Uuid>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (send_confirmed_tx, send_confirmed_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, spawn_result, mut child) =
            if is_parent_after_async_spawn(&first_request) {
                (first, first_request, second)
            } else {
                (second, second_request, first)
            };
        let agent_id = first_uuid(&spawn_result).expect("spawn result contains agent UUID");
        respond(
            &mut parent_after_spawn,
            &wait_turn("call_wait_initial", agent_id, 2_000),
        )
        .await;
        respond(&mut child, &metered_text_turn("initial child complete")).await;

        let (mut parent_after_initial, _) =
            listener.accept().await.expect("parent after initial child");
        let initial_result = read_request(&mut parent_after_initial).await;
        assert!(initial_result.contains("initial child complete"));
        respond(
            &mut parent_after_initial,
            &send_turn_with_key(
                "call_send_before_crash",
                agent_id,
                "Recover this accepted follow-up.",
                "durable-followup-1",
            ),
        )
        .await;

        let (mut first_after_send, _) = listener
            .accept()
            .await
            .expect("first accepted-send connection");
        let first_after_send_request = read_request(&mut first_after_send).await;
        let (mut second_after_send, _) = listener
            .accept()
            .await
            .expect("second accepted-send connection");
        let second_after_send_request = read_request(&mut second_after_send).await;
        let (_parent_after_send, send_result, mut interrupted_child) =
            if first_after_send_request.contains("submission_id") {
                (
                    first_after_send,
                    first_after_send_request,
                    second_after_send,
                )
            } else {
                (
                    second_after_send,
                    second_after_send_request,
                    first_after_send,
                )
            };
        assert!(send_result.contains("durable-followup-1"));
        assert!(send_result.contains(&format!("{agent_id}:1")));
        interrupted_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start interrupted child stream");
        interrupted_child.flush().await.expect("flush child stream");
        let _ = send_confirmed_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if interrupted_child
                .write_all(b": waiting-for-crash\n\n")
                .await
                .is_err()
                || interrupted_child.flush().await.is_err()
            {
                break;
            }
        }

        let (mut first_recovered, _) = listener.accept().await.expect("first recovered connection");
        let first_recovered_request = read_request(&mut first_recovered).await;
        let (mut second_recovered, _) = listener
            .accept()
            .await
            .expect("second recovered connection");
        let second_recovered_request = read_request(&mut second_recovered).await;
        let (mut recovered_parent, mut recovered_child) =
            if first_recovered_request.contains("submission_id") {
                (first_recovered, second_recovered)
            } else {
                assert!(second_recovered_request.contains("submission_id"));
                (second_recovered, first_recovered)
            };
        let recovered_child_request = if first_recovered_request.contains("submission_id") {
            second_recovered_request
        } else {
            first_recovered_request
        };
        assert!(recovered_child_request.contains("Recover this accepted follow-up."));
        assert_eq!(
            conversation_messages(&recovered_child_request),
            vec![
                (
                    "user".into(),
                    "Keep working until the shared deadline stops you.".into(),
                ),
                ("assistant".into(), "initial child complete".into()),
                ("user".into(), "Recover this accepted follow-up.".into()),
            ],
            "Host replacement must resume the exact role-preserving child history"
        );
        respond(
            &mut recovered_parent,
            &wait_turn("call_wait_recovered_send", agent_id, 2_000),
        )
        .await;
        respond(
            &mut recovered_child,
            &metered_text_turn("accepted follow-up recovered once"),
        )
        .await;

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        assert!(final_request.contains("accepted follow-up recovered once"));
        respond(
            &mut parent_final,
            &text_turn("parent recovered confirmed send"),
        )
        .await;
        agent_id
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        send_confirmed_rx,
        provider,
    )
}

async fn crash_after_confirmed_send_checkpoint(
    config: LocalRuntimeConfig,
    run_id: Uuid,
    send_confirmed: oneshot::Receiver<()>,
) -> (Uuid, String) {
    tokio::task::spawn_blocking(move || {
        let state_root = config.state_root.clone();
        let runtime = tokio::runtime::Runtime::new().expect("isolated runtime");
        runtime.block_on(async move {
            let mut host = LocalRuntimeHost::start(config).expect("first host");
            let execution = tokio::spawn(async move {
                host.execute_as(run_id, "Delegate the bounded child.").await
            });
            tokio::time::timeout(Duration::from_secs(5), send_confirmed)
                .await
                .expect("send was not confirmed before crash")
                .expect("provider dropped send confirmation");
            let checkpoint_path = LocalRuntimeHost::checkpoint_path(&state_root, run_id);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let durable = loop {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "confirmed message receipt did not become durable"
                );
                let receipt = checkpoint_path
                    .is_file()
                    .then(|| LocalRuntimeHost::load_checkpoint(&checkpoint_path).ok())
                    .flatten()
                    .and_then(|snapshot| {
                        serde_json::from_slice::<serde_json::Value>(&snapshot.state).ok()
                    })
                    .and_then(|state| {
                        let receipts = state["subagent_message_receipts"].as_object()?;
                        let (agent_id, by_key) = receipts.iter().next()?;
                        let receipt = by_key.as_object()?.values().next()?;
                        Some((
                            Uuid::parse_str(agent_id).ok()?,
                            receipt["submission_id"].as_str()?.to_owned(),
                        ))
                    });
                if let Some(durable) = receipt {
                    break durable;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            execution.abort();
            durable
        })
    })
    .await
    .expect("crash runtime thread")
}

async fn spawn_parent_finishes_with_live_child_provider() -> (String, tokio::task::JoinHandle<bool>)
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent spawn turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &async_spawn_turn(20)).await;

        let (mut first, _) = listener.accept().await.expect("first post-spawn turn");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second post-spawn turn");
        let second_request = read_request(&mut second).await;
        let (mut parent_after_spawn, mut child) = if is_parent_after_async_spawn(&first_request) {
            (first, second)
        } else {
            assert!(is_parent_after_async_spawn(&second_request));
            (second, first)
        };
        respond(
            &mut parent_after_spawn,
            &text_turn("parent intentionally finishes without waiting"),
        )
        .await;

        tokio::time::timeout(Duration::from_secs(1), async {
            let mut byte = [0_u8; 1];
            loop {
                match child.read(&mut byte).await {
                    Ok(0) | Err(_) => break true,
                    Ok(_) => continue,
                }
            }
        })
        .await
        .unwrap_or(false)
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn crash_after_async_handle_checkpoint(
    config: LocalRuntimeConfig,
    run_id: Uuid,
    connections_established: oneshot::Receiver<()>,
    crash_started: oneshot::Sender<()>,
    crash_observed: oneshot::Receiver<()>,
    connections_closed: oneshot::Receiver<()>,
) -> Uuid {
    let state_root = config.state_root.clone();
    let mut host = LocalRuntimeHost::start(config).expect("first host");
    let mut execution =
        tokio::spawn(async move { host.execute_as(run_id, "Delegate the bounded child.").await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let agent_id = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "async handle and child checkpoint did not become durable"
        );
        let parent_checkpoint = LocalRuntimeHost::checkpoint_path(&state_root, run_id);
        let active = parent_checkpoint
            .is_file()
            .then(|| LocalRuntimeHost::load_checkpoint(&parent_checkpoint).ok())
            .flatten()
            .and_then(|snapshot| serde_json::from_slice::<serde_json::Value>(&snapshot.state).ok())
            .and_then(|state| {
                state["active_subagents"]
                    .as_object()
                    .and_then(|active| active.keys().find_map(|key| Uuid::parse_str(key).ok()))
            });
        if let Some(agent_id) = active
            && LocalRuntimeHost::checkpoint_path(&state_root, agent_id).is_file()
        {
            break agent_id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    tokio::time::timeout(Duration::from_secs(5), connections_established)
        .await
        .expect("Provider did not establish the parent and child connections before crash")
        .expect("Provider dropped before both connections were established");
    execution.abort();
    tokio::time::timeout(Duration::from_secs(5), &mut execution)
        .await
        .expect("aborted Host execution task did not finish dropping its resources")
        .expect_err("aborted Host execution unexpectedly completed");
    crash_started
        .send(())
        .expect("Provider dropped before the simulated crash signal");
    tokio::time::timeout(Duration::from_secs(5), crash_observed)
        .await
        .expect("Provider did not observe the simulated crash")
        .expect("Provider dropped the crash acknowledgement");
    connections_closed
        .await
        .expect("aborted Host retained a Provider connection");
    agent_id
}

async fn spawn_deadline_blocking_provider()
-> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (closed_tx, closed_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &single_spawn_turn(1)).await;

        let (mut child, _) = listener.accept().await.expect("child turn");
        let child_request = read_request(&mut child).await;
        assert!(child_request.contains("Keep working until the shared deadline stops you."));
        child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start child stream");
        child.flush().await.expect("flush child stream");
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
        closed_rx,
        provider,
    )
}

async fn spawn_partial_batch_recovery_provider()
-> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let (batch_started_tx, batch_started_rx) = oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &parallel_spawn_turn()).await;

        let (mut completed_child, _) = listener.accept().await.expect("completed child");
        let completed_request = read_request(&mut completed_child).await;
        let completed_is_alpha = completed_request.contains("Solve alpha independently.");
        let completed_text = if completed_is_alpha {
            "alpha solved before crash"
        } else {
            "beta solved before crash"
        };
        let (mut blocked_child, _) = listener.accept().await.expect("blocked child");
        let blocked_request = read_request(&mut blocked_child).await;
        assert_ne!(
            completed_is_alpha,
            blocked_request.contains("Solve alpha independently.")
        );
        respond(&mut completed_child, &metered_text_turn(completed_text)).await;
        blocked_child
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .expect("start blocked child stream");
        blocked_child.flush().await.expect("flush blocked child");
        let _ = batch_started_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if blocked_child
                .write_all(b": blocked-before-crash\n\n")
                .await
                .is_err()
                || blocked_child.flush().await.is_err()
            {
                break;
            }
        }

        let (mut recovered_child, _) = listener.accept().await.expect("recovered child");
        let recovered_request = read_request(&mut recovered_child).await;
        let blocked_is_alpha = !completed_is_alpha;
        assert_eq!(
            recovered_request.contains("Solve alpha independently."),
            blocked_is_alpha,
            "recovery must invoke only the child that had no durable result receipt"
        );
        assert_ne!(
            recovered_request.contains("Solve alpha independently."),
            completed_request.contains("Solve alpha independently."),
            "the completed child must not be replayed"
        );
        let recovered_text = if blocked_is_alpha {
            "alpha solved after recovery"
        } else {
            "beta solved after recovery"
        };
        respond(&mut recovered_child, &metered_text_turn(recovered_text)).await;

        let (mut parent_final, _) = listener.accept().await.expect("recovered parent");
        let final_request = read_request(&mut parent_final).await;
        for expected in ["call_alpha", "call_beta", completed_text, recovered_text] {
            assert!(
                final_request.contains(expected),
                "recovered parent did not receive {expected}: {final_request}"
            );
        }
        respond(&mut parent_final, &text_turn("recovered parent result")).await;
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        batch_started_rx,
        provider,
    )
}

async fn spawn_one_child_failure_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &parallel_spawn_turn()).await;

        let (mut first, _) = listener.accept().await.expect("first child");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second child");
        let second_request = read_request(&mut second).await;
        for (socket, request) in [(&mut first, first_request), (&mut second, second_request)] {
            if request.contains("Solve alpha independently.") {
                respond(socket, &metered_text_turn("alpha succeeded")).await;
            } else {
                assert!(request.contains("Solve beta independently."));
                respond(socket, &content_filter_turn()).await;
            }
        }

        let (mut parent_final, _) = listener.accept().await.expect("parent final turn");
        let final_request = read_request(&mut parent_final).await;
        for expected in [
            "call_alpha",
            "call_beta",
            "alpha succeeded",
            "failed",
            "is_error",
        ] {
            assert!(
                final_request.contains(expected),
                "parent did not receive child failure metadata {expected}: {final_request}"
            );
        }
        respond(
            &mut parent_final,
            &text_turn("parent handled the child failure"),
        )
        .await;
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

async fn spawn_approval_ordering_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let port = listener.local_addr().expect("provider address").port();
    let provider = tokio::spawn(async move {
        let (mut parent, _) = listener.accept().await.expect("parent first turn");
        let _ = read_request(&mut parent).await;
        respond(&mut parent, &parallel_approval_spawn_turn()).await;

        let (mut first, _) = listener.accept().await.expect("first child");
        let first_request = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.expect("second child");
        let second_request = read_request(&mut second).await;
        for (socket, request) in [(&mut first, first_request), (&mut second, second_request)] {
            if request.contains("Alpha must read evidence.") {
                respond(socket, &workspace_read_turn()).await;
            } else {
                assert!(request.contains("Beta completes without tools."));
                respond(socket, &metered_text_turn("beta finished early")).await;
            }
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        provider,
    )
}

fn runtime_config(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    endpoint: String,
) -> LocalRuntimeConfig {
    let mut config = LocalRuntimeConfig {
        state_root: state_root.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
        agent_instructions: "Delegate both independent tasks in parallel.".into(),
        delegated_scopes: BTreeSet::from(["agent:spawn".into()]),
        subagent_roles: vec![SubagentRole {
            name: "worker".into(),
            instructions: "Solve only the assigned independent task.".into(),
            delegated_scopes: BTreeSet::new(),
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

#[tokio::test]
async fn two_subagents_are_inflight_before_either_child_completes() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_parallel_provider().await;
    let local_config = LocalRuntimeConfig {
        state_root: state.path().to_path_buf(),
        workspace_root: workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        agent_instructions: "Delegate both independent tasks in parallel.".into(),
        delegated_scopes: BTreeSet::from(["agent:spawn".into()]),
        subagent_roles: vec![SubagentRole {
            name: "worker".into(),
            instructions: "Solve only the assigned independent task.".into(),
            delegated_scopes: BTreeSet::new(),
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
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    };
    let mut host = LocalRuntimeHost::start(local_config.clone()).expect("start host");

    let outcome = host
        .execute("Delegate alpha and beta.")
        .await
        .expect("parallel parent run");
    let observed_parallel = provider.await.expect("provider task");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        observed_parallel,
        "both child model turns must be accepted before either child response is released"
    );
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.spawn.requested")
            .count(),
        2
    );
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.result.received")
            .count(),
        2
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("parent checkpoint with settled child usage");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        checkpoint["budget_usage"]["tokens"], 300,
        "both child usage receipts must be settled into the parent budget"
    );
    let child_run_ids = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0)
        .expect("parent event log")
        .into_iter()
        .filter(|event| event.event_type == "subagent.spawn.requested")
        .filter_map(|event| {
            event
                .payload
                .get("request")
                .and_then(|request| request.get("delegation_id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(child_run_ids.len(), 2);
    for child_run_id in &child_run_ids {
        let record = LocalRuntimeHost::read_run_record(state.path(), *child_run_id)
            .expect("child Run record read")
            .expect("child Run is retention-managed");
        assert!(matches!(record.state, LocalRunState::Finished { .. }));
    }
    drop(host);
    let runtime = EmbeddedRuntime::new_with_retention(
        RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 8,
            max_queued_runs_per_tenant: 8,
        },
        vec![RuntimeProfile {
            invocation: local_invocation_context(),
            config: local_config,
        }],
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 8,
            max_run_directories_per_tenant: 16,
            retain_terminal_runs_per_workspace: 0,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 16,
            max_run_tombstones_per_tenant: 32,
            max_control_tombstones_per_workspace: 16,
            max_control_tombstones_per_tenant: 32,
        },
    )
    .expect("retention Runtime");
    let report = runtime
        .maintain_retention(local_invocation_context())
        .expect("completed child retention");
    assert_eq!(report.tombstoned_runs, 2);
    assert_eq!(report.strongly_referenced_runs, 0);
    assert_eq!(report.unmanaged_run_directories, 1);
    for child_run_id in child_run_ids {
        assert!(
            !state
                .path()
                .join("runs")
                .join(child_run_id.to_string())
                .exists()
        );
    }
    assert!(
        outcome.checkpoint_path.exists(),
        "the parent result graph remains durable after child hot artifacts are retired"
    );
}

#[tokio::test]
async fn spawn_returns_a_persistent_handle_before_wait_observes_child_completion() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_async_handle_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = host
        .execute("Delegate the bounded child.")
        .await
        .expect("asynchronous subagent lifecycle");
    let observed_async_spawn = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert!(
        observed_async_spawn,
        "agent.spawn waited for child completion instead of returning a stable handle"
    );
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent observed async child");
}

#[tokio::test]
async fn close_cancels_only_the_targeted_asynchronous_child_and_reaps_its_stream() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_async_close_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = host
        .execute("Delegate the bounded child.")
        .await
        .expect("close lifecycle");
    let observed_close = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert!(
        observed_close,
        "agent.close did not reap the active child stream"
    );
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent closed async child");
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.closed")
            .count(),
        1,
        "the Host must durably record the irreversible close edge"
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("parent checkpoint after close");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        checkpoint["closed_subagents"].as_array().map(Vec::len),
        Some(1),
        "the closed handle must survive Host replacement"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_host_recovers_the_same_async_handle_without_replaying_spawn() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (
        endpoint,
        connections_established,
        crash_started,
        crash_observed,
        connections_closed,
        provider,
    ) = spawn_async_recovery_provider().await;
    let config = runtime_config(state.path(), &workspace_root, endpoint);
    let run_id = Uuid::now_v7();

    let checkpoint_agent_id = tokio::time::timeout(
        Duration::from_secs(15),
        crash_after_async_handle_checkpoint(
            config.clone(),
            run_id,
            connections_established,
            crash_started,
            crash_observed,
            connections_closed,
        ),
    )
    .await
    .expect("first Host did not release its Provider connections after abort");
    let mut replacement = LocalRuntimeHost::start(config).expect("replacement host");
    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        replacement.resume(run_id, "Delegate the bounded child.", 2),
    )
    .await
    .expect("replacement Host did not finish the recovered parent/child turns")
    .expect("recover asynchronous handle");
    let provider_agent_id = tokio::time::timeout(Duration::from_secs(15), provider)
        .await
        .expect("Provider lifecycle did not finish after recovered Run")
        .expect("provider lifecycle");
    tokio::time::timeout(Duration::from_secs(5), replacement.shutdown())
        .await
        .expect("replacement Host did not shut down");

    assert_eq!(checkpoint_agent_id, provider_agent_id);
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent recovered async handle");
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.spawn.requested")
            .count(),
        0,
        "replacement Host must restore the handle instead of replaying spawn"
    );
}

#[tokio::test]
async fn send_starts_a_followup_turn_under_the_same_persistent_agent_handle() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_async_send_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = host
        .execute("Delegate the bounded child.")
        .await
        .expect("persistent child dialogue");
    let _agent_id = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent completed persistent dialog");
}

#[tokio::test]
async fn different_handles_share_one_parent_budget_in_the_real_host_loop() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_tree_budget_provider().await;
    let mut config = runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    );
    config.budget = RunBudget {
        max_tokens: 700,
        max_cost_cents: 50,
        max_duration_seconds: 30,
    };
    let mut host = LocalRuntimeHost::start(config).expect("start host");

    let outcome = host
        .execute("Create two durable handles, then continue both under one budget.")
        .await
        .expect("tree-wide budget run");
    let observed_child_token_caps = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "tree budget verified");
    assert_eq!(
        observed_child_token_caps,
        vec![300, 400],
        "the second handle must receive only the unreserved parent balance"
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("terminal budget checkpoint");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(checkpoint["schema_version"], 27);
    assert_eq!(
        checkpoint["subagent_budget_reservations"]
            .as_object()
            .map(serde_json::Map::len),
        Some(0),
        "terminal child results must leave no stranded reservation"
    );
}

#[tokio::test]
async fn fork_creates_an_independent_bounded_handle_from_a_completed_turn() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "fork source evidence",
    )
    .expect("write fork evidence");
    let (endpoint, provider) = spawn_async_fork_provider().await;
    let mut config = runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    );
    config
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.subagent_roles[0]
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.trusted_workspace_tool = Some(trusted_tool);
    config.consent = LocalToolConsent::AllowOnce;
    let mut host = LocalRuntimeHost::start(config).expect("start host");

    let run_id = Uuid::now_v7();
    let outcome = host
        .execute_as(run_id, "Delegate and fork the bounded child.")
        .await;
    host.shutdown().await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            provider.abort();
            let _ = provider.await;
            let event_types = LocalRuntimeHost::replay_events(state.path(), run_id, 0)
                .unwrap_or_default()
                .into_iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>();
            panic!(
                "fork lifecycle failed before the provider completed: {error}; events={event_types:?}"
            );
        }
    };
    let (source_agent_id, forked_agent_id) = provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent completed fork lifecycle");
    assert_ne!(source_agent_id, forked_agent_id);
    let events =
        LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).expect("parent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.forked")
            .count(),
        1
    );
    let child_tool_executions = std::fs::read_dir(state.path().join("runs"))
        .expect("run directories")
        .filter_map(Result::ok)
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .filter(|run_id| *run_id != outcome.run_id)
        .flat_map(|run_id| {
            LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap_or_default()
        })
        .filter(|event| event.event_type == "tool.execution.started")
        .count();
    assert_eq!(
        child_tool_executions, 1,
        "fork continuation replayed the source workspace Tool instead of inheriting its result"
    );
}

#[tokio::test]
async fn rollback_preserves_the_old_generation_and_continues_from_a_fenced_head() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "rollback source evidence",
    )
    .expect("write rollback evidence");
    let (endpoint, provider) = spawn_async_rollback_provider().await;
    let mut config = runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    );
    config
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.subagent_roles[0]
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.trusted_workspace_tool = Some(trusted_tool);
    config.consent = LocalToolConsent::AllowOnce;
    let mut host = LocalRuntimeHost::start(config).expect("start host");

    let run_id = Uuid::now_v7();
    let outcome = host
        .execute_as(
            run_id,
            "Delegate, roll back, and continue the bounded child.",
        )
        .await;
    host.shutdown().await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            provider.abort();
            let _ = provider.await;
            let event_types = LocalRuntimeHost::replay_events(state.path(), run_id, 0)
                .unwrap_or_default()
                .into_iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>();
            panic!(
                "rollback lifecycle failed before the provider completed: {error}; events={event_types:?}"
            );
        }
    };
    let agent_id = provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent completed rollback lifecycle");
    let events =
        LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).expect("parent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.rolled_back")
            .count(),
        1
    );
    assert!(events.iter().any(|event| {
        event.event_type == "subagent.rolled_back"
            && event.payload["agent_id"] == agent_id.to_string()
            && event.payload["from_generation"] == 1
            && event.payload["generation"] == 2
    }));
    let child_tool_executions = std::fs::read_dir(state.path().join("runs"))
        .expect("run directories")
        .filter_map(Result::ok)
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .filter(|run_id| *run_id != outcome.run_id)
        .flat_map(|run_id| {
            LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap_or_default()
        })
        .filter(|event| event.event_type == "tool.execution.started")
        .count();
    assert_eq!(
        child_tool_executions, 1,
        "rollback replayed the retained workspace Tool instead of inheriting its result"
    );
}

#[tokio::test]
async fn followup_inherits_the_child_assistant_tool_call_and_tool_result() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "durable child evidence",
    )
    .expect("write evidence");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, provider) = spawn_async_tool_history_provider().await;
    let mut config = runtime_config(state.path(), &workspace_root, endpoint);
    config
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.subagent_roles[0]
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.trusted_workspace_tool = Some(trusted_tool);
    config.consent = LocalToolConsent::AllowOnce;
    let mut host = LocalRuntimeHost::start(config).expect("start host");

    let outcome = tokio::time::timeout(
        Duration::from_secs(8),
        host.execute("Delegate the bounded child."),
    )
    .await
    .expect("Tool-backed child dialogue timed out")
    .expect("Tool-backed child dialogue");
    let agent_id = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent preserved the child Tool transcript");
    let child_checkpoint = LocalRuntimeHost::load_checkpoint(&LocalRuntimeHost::checkpoint_path(
        state.path(),
        agent_id,
    ))
    .expect("child terminal checkpoint");
    assert_eq!(child_checkpoint.status, RunStatus::Succeeded);
    let terminal_transcript =
        WorkerProcessor::conversation_transcript_from_checkpoint(&child_checkpoint)
            .expect("child terminal transcript remains recoverable after shutdown");
    assert!(terminal_transcript.iter().any(|message| {
        message.role == agent_protocol::Role::Tool
            && matches!(
                message.content.as_slice(),
                [agent_protocol::ContentPart::ToolResult { tool_call_id, .. }]
                    if tool_call_id == "call_history_read"
            )
    }));
}

#[tokio::test]
async fn replacement_recovers_child_tool_history_from_the_terminal_checkpoint() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("EVIDENCE.txt"),
        "durable child evidence",
    )
    .expect("write evidence");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, child_active, release_child, provider) =
        spawn_terminal_gap_recovery_provider().await;
    let mut config = runtime_config(state.path(), &workspace_root, endpoint);
    config
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.subagent_roles[0]
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.trusted_workspace_tool = Some(trusted_tool);
    config.consent = LocalToolConsent::AllowOnce;
    let run_id = Uuid::now_v7();
    let mut first_host = LocalRuntimeHost::start(config.clone()).expect("first host");
    let parent_checkpoint_path = LocalRuntimeHost::checkpoint_path(state.path(), run_id);
    let parent_events_path = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl");
    let (first_outcome, agent_id, active_checkpoint, active_events) = {
        let first_execution = first_host.execute_as(run_id, "Delegate the bounded child.");
        tokio::pin!(first_execution);
        let mut child_active = child_active;
        let agent_id = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::select! {
                agent_id = &mut child_active => agent_id.expect("child reached active state"),
                outcome = first_execution.as_mut() => panic!(
                    "parent completed before the child became active: {outcome:?}"
                ),
            }
        })
        .await
        .expect("child did not reach active state");
        let active_checkpoint =
            std::fs::read(&parent_checkpoint_path).expect("active parent checkpoint");
        let active_events = std::fs::read(&parent_events_path).expect("active parent event log");
        let active_snapshot = LocalRuntimeHost::load_checkpoint(&parent_checkpoint_path)
            .expect("decode active parent checkpoint");
        let active_state: serde_json::Value =
            serde_json::from_slice(&active_snapshot.state).expect("active checkpoint state");
        assert!(
            active_state["active_subagents"]
                .get(agent_id.to_string())
                .is_some()
        );
        release_child.send(()).expect("release child");
        let first_outcome = tokio::time::timeout(Duration::from_secs(8), first_execution.as_mut())
            .await
            .expect("first process did not reach its terminal result")
            .expect("first process completes the child once");
        (first_outcome, agent_id, active_checkpoint, active_events)
    };
    assert_eq!(first_outcome.status, RunStatus::Succeeded);
    first_host.shutdown().await;
    let child_checkpoint = LocalRuntimeHost::load_checkpoint(&LocalRuntimeHost::checkpoint_path(
        state.path(),
        agent_id,
    ))
    .expect("child terminal checkpoint");
    assert_eq!(child_checkpoint.status, RunStatus::Succeeded);
    let child_events_path = state
        .path()
        .join("runs")
        .join(agent_id.to_string())
        .join("events.jsonl");
    let mut child_events = std::fs::read_to_string(&child_events_path)
        .expect("completed child event log")
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let unpublished: serde_json::Value = serde_json::from_str(
        &child_events
            .pop()
            .expect("completed child has a terminal event"),
    )
    .expect("terminal child event JSON");
    assert_eq!(unpublished["type"], "run.succeeded");
    std::fs::write(&child_events_path, format!("{}\n", child_events.join("\n")))
        .expect("recreate child Checkpoint-before-Event crash window");

    std::fs::write(&parent_checkpoint_path, active_checkpoint)
        .expect("restore the exact pre-result parent checkpoint");
    std::fs::write(&parent_events_path, active_events)
        .expect("restore the exact pre-result parent event prefix");
    let result_path = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("subagents")
        .join(format!("{agent_id}.result.json"));
    std::fs::remove_file(result_path).expect("remove result written after the simulated crash");

    let mut replacement = LocalRuntimeHost::start(config).expect("replacement host");
    let outcome = tokio::time::timeout(
        Duration::from_secs(8),
        replacement.resume(run_id, "Delegate the bounded child.", 2),
    )
    .await
    .expect("replacement recovery timed out")
    .expect("replacement recovers terminal child");
    let provider_agent_id = provider.await.expect("provider lifecycle");
    replacement.shutdown().await;

    assert_eq!(provider_agent_id, agent_id);
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome.output,
        "parent recovered the terminal Tool transcript"
    );
}

#[tokio::test]
async fn send_queues_behind_a_running_child_and_promotes_in_fifo_order() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_async_mailbox_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = host
        .execute("Delegate the bounded child.")
        .await
        .expect("persistent mailbox lifecycle");
    let agent_id = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent drained persistent mailbox");
    let events =
        LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).expect("parent event log");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.accepted")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.activated")
            .count(),
        1
    );
    let checkpoint =
        LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).expect("mailbox checkpoint");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(checkpoint["schema_version"], 27);
    assert_eq!(
        checkpoint["subagent_message_queues"][agent_id.to_string()]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}

#[tokio::test]
async fn interrupting_send_checkpoints_then_replaces_the_running_child_turn() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_async_interrupt_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        host.execute("Delegate the bounded child."),
    )
    .await
    .expect("durable interrupt did not settle")
    .expect("durable interrupt lifecycle");
    let agent_id = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent completed durable interrupt");
    let events =
        LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).expect("parent event log");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.accepted")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.activated")
            .count(),
        1
    );
    let checkpoint =
        LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).expect("interrupt checkpoint");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        checkpoint["subagent_message_receipts"][agent_id.to_string()]["interrupt-active-1"]["interrupt"],
        true
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpointed_interrupt_survives_host_replacement_without_restarting_old_work() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, provider) = spawn_async_interrupt_recovery_provider().await;
    let config = runtime_config(state.path(), &workspace_root, endpoint);
    let run_id = Uuid::now_v7();

    let checkpoint_agent_id = crash_after_interrupt_receipt(config.clone(), run_id).await;
    let mut replacement = LocalRuntimeHost::start(config).expect("replacement host");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        replacement.resume(run_id, "Delegate the bounded child.", 2),
    )
    .await
    .expect("replacement did not settle the durable interrupt")
    .expect("resume durable interrupt");
    let provider_agent_id = provider.await.expect("provider lifecycle");
    replacement.shutdown().await;

    assert_eq!(checkpoint_agent_id, provider_agent_id);
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent recovered durable interrupt");
    let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).expect("parent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.accepted")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.activated")
            .count(),
        1
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("checkpoint after recovered interrupt");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        checkpoint["subagent_message_receipts"][checkpoint_agent_id.to_string()]["interrupt-recovery-1"]
            ["interrupt"],
        true
    );
    assert_eq!(
        checkpoint["budget_usage"]["tokens"], 150,
        "only the redirected child may settle model usage after recovery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_confirmed_send_survives_host_replacement_without_a_second_message_receipt() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, send_confirmed, provider) = spawn_async_send_recovery_provider().await;
    let config = runtime_config(state.path(), &workspace_root, endpoint);
    let run_id = Uuid::now_v7();

    let (checkpoint_agent_id, submission_id) =
        crash_after_confirmed_send_checkpoint(config.clone(), run_id, send_confirmed).await;
    let mut replacement = LocalRuntimeHost::start(config).expect("replacement host");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        replacement.resume(run_id, "Delegate the bounded child.", 2),
    )
    .await
    .expect("replacement did not resume the confirmed send")
    .expect("resume confirmed send");
    let provider_agent_id = provider.await.expect("provider lifecycle");
    replacement.shutdown().await;

    assert_eq!(checkpoint_agent_id, provider_agent_id);
    assert_eq!(submission_id, format!("{checkpoint_agent_id}:1"));
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "parent recovered confirmed send");
    let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).expect("parent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.input.accepted")
            .count(),
        1,
        "replacement must recover the receipt instead of accepting the message twice"
    );
    let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path)
        .expect("checkpoint after recovered send");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(
        checkpoint["subagent_message_receipts"][checkpoint_agent_id.to_string()]
            .as_object()
            .map(serde_json::Map::len),
        Some(1)
    );
    assert_eq!(
        checkpoint["budget_usage"]["tokens"], 300,
        "the interrupted and recovered successor must settle its usage exactly once"
    );
}

#[tokio::test]
async fn parent_terminal_state_cancels_and_reaps_unclosed_async_children() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_parent_finishes_with_live_child_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace.path().canonicalize().expect("workspace"),
        endpoint,
    ))
    .expect("start host");

    let outcome = host
        .execute("Delegate the bounded child.")
        .await
        .expect("parent terminal outcome");
    let child_reaped = provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert!(
        child_reaped,
        "a parent terminal state left an asynchronous child connection alive"
    );
}

#[tokio::test]
async fn cancelling_the_parent_closes_every_inflight_child_in_the_batch() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, started, closed, provider) = spawn_parallel_blocking_provider().await;
    let cancellation = CancellationToken::new();
    let mut host = LocalRuntimeHost::start_with_cancellation(
        LocalRuntimeConfig {
            state_root: state.path().to_path_buf(),
            workspace_root: workspace.path().canonicalize().expect("workspace"),
            agent_instructions: "Delegate both independent tasks in parallel.".into(),
            delegated_scopes: BTreeSet::from(["agent:spawn".into()]),
            subagent_roles: vec![SubagentRole {
                name: "worker".into(),
                instructions: "Solve only the assigned independent task.".into(),
                delegated_scopes: BTreeSet::new(),
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
            consent: LocalToolConsent::Ask,
            budget: RunBudget {
                max_tokens: 4_096,
                max_cost_cents: 100,
                max_duration_seconds: 600,
            },
            runtime_policy: RuntimeExecutionPolicySnapshot::default(),
        },
        cancellation.clone(),
    )
    .expect("start host");
    let execution = tokio::spawn(async move {
        let outcome = host.execute("Delegate alpha and beta.").await;
        host.shutdown().await;
        outcome
    });
    tokio::time::timeout(Duration::from_secs(3), started)
        .await
        .expect("both child streams did not start")
        .expect("child start signal dropped");
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .expect("one or more child streams survived cancellation")
        .expect("child close signal dropped");
    let outcome = execution.await.expect("execution task").expect("outcome");
    provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.spawn.requested")
            .count(),
        2
    );
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.cancelled")
            .count(),
        1
    );
}

#[tokio::test]
async fn parent_duration_budget_stops_a_streaming_child_and_terminalizes_the_tree() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, child_closed, provider) = spawn_deadline_blocking_provider().await;
    let mut config = runtime_config(state.path(), &workspace_root, endpoint);
    config.budget.max_duration_seconds = 1;
    let mut host = LocalRuntimeHost::start(config).expect("start host");

    let execution = tokio::time::timeout(
        Duration::from_millis(2_500),
        host.execute("Delegate the bounded child."),
    )
    .await;
    let outcome = match execution {
        Ok(outcome) => outcome.expect("duration-limited outcome"),
        Err(_) => {
            provider.abort();
            host.shutdown().await;
            panic!("the parent duration budget did not stop the active child tree");
        }
    };
    tokio::time::timeout(Duration::from_secs(1), child_closed)
        .await
        .expect("child stream survived the parent duration deadline")
        .expect("child close signal dropped");
    provider.await.expect("provider lifecycle");
    host.shutdown().await;

    assert_eq!(outcome.status, RunStatus::TimedOut);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "run.timed_out")
            .count(),
        1,
        "the root Run must publish exactly one timeout terminal event"
    );
}

#[tokio::test]
async fn recovery_reuses_completed_batch_receipts_and_restarts_only_unfinished_children() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, batch_started, provider) = spawn_partial_batch_recovery_provider().await;
    let run_id = Uuid::now_v7();
    let mut first = LocalRuntimeHost::start(runtime_config(
        state.path(),
        &workspace_root,
        endpoint.clone(),
    ))
    .expect("start first host");
    let first_execution =
        tokio::spawn(async move { first.execute_as(run_id, "Delegate alpha and beta.").await });
    tokio::time::timeout(Duration::from_secs(3), batch_started)
        .await
        .expect("parallel batch did not start")
        .expect("batch start signal dropped");
    let receipt_dir = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("subagents");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::read_dir(&receipt_dir)
                .ok()
                .is_some_and(|entries| entries.filter_map(Result::ok).count() == 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed child result was not persisted before the crash");
    first_execution.abort();
    let _ = first_execution.await;

    let mut replacement =
        LocalRuntimeHost::start(runtime_config(state.path(), &workspace_root, endpoint))
            .expect("start replacement host");
    let outcome = replacement
        .resume(run_id, "Delegate alpha and beta.", 2)
        .await
        .expect("resume partial batch");
    replacement.shutdown().await;
    provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).expect("parent events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.spawn.requested")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "subagent.result.received")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_dir(receipt_dir)
            .expect("receipt directory")
            .filter_map(Result::ok)
            .count(),
        2
    );
}

#[tokio::test]
async fn one_failed_child_is_bound_as_an_error_without_losing_its_successful_sibling() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, provider) = spawn_one_child_failure_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(state.path(), &workspace_root, endpoint))
        .expect("start host");
    let outcome = host
        .execute("Delegate alpha and beta.")
        .await
        .expect("parent handles bounded child failure");
    host.shutdown().await;
    provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.result.received")
            .count(),
        2
    );
    let child_terminal_events = std::fs::read_dir(state.path().join("runs"))
        .expect("run directories")
        .filter_map(Result::ok)
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .filter(|run_id| *run_id != outcome.run_id)
        .flat_map(|run_id| {
            LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap_or_default()
        })
        .filter(|event| matches!(event.event_type.as_str(), "run.succeeded" | "run.failed"))
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(child_terminal_events.contains(&"run.succeeded".into()));
    assert!(child_terminal_events.contains(&"run.failed".into()));
}

#[tokio::test]
async fn a_later_completed_child_waits_behind_an_earlier_child_approval() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("EVIDENCE.txt"), "approval evidence")
        .expect("write evidence");
    let workspace_root = workspace.path().canonicalize().expect("workspace");
    let (endpoint, provider) = spawn_approval_ordering_provider().await;
    let mut config = runtime_config(state.path(), &workspace_root, endpoint);
    config
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.subagent_roles[0]
        .delegated_scopes
        .insert(WORKSPACE_READ_SCOPE.to_owned());
    config.trusted_workspace_tool = Some(trusted_tool);
    let mut host = LocalRuntimeHost::start(config).expect("start host");
    let outcome = host
        .execute("Delegate alpha and beta.")
        .await
        .expect("park parent on child approval");
    host.shutdown().await;
    provider.await.expect("provider lifecycle");

    assert_eq!(outcome.status, RunStatus::Suspended);
    assert!(outcome.pending_approval.is_some());
    assert_eq!(
        outcome
            .event_types
            .iter()
            .filter(|event| event.as_str() == "subagent.result.received")
            .count(),
        0,
        "the later beta result must not cross the earlier alpha approval barrier"
    );
    let checkpoint =
        LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).expect("parent checkpoint");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(checkpoint["pending_subagents"].as_array().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_dir(
            state
                .path()
                .join("runs")
                .join(outcome.run_id.to_string())
                .join("subagents")
        )
        .expect("durable beta receipt")
        .filter_map(Result::ok)
        .count(),
        1,
        "the later result stays durable even though the parent has not consumed it"
    );
}
