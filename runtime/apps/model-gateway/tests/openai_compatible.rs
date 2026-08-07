use agent_model_gateway::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing,
};
use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role, ToolSpec,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct CapturedRequest {
    head: String,
    body: Value,
}

fn model_request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "say hello".into(),
            }],
        }],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        }],
        output_schema: None,
        reasoning: ReasoningPolicy::Balanced,
        max_output_tokens: 64,
    }
}

fn config(endpoint: String) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        endpoint,
        model: "local-agent-model".into(),
        pricing: ProviderPricing {
            input_million_tokens_micros: 1_000_000,
            output_million_tokens_micros: 2_000_000,
        },
        response_timeout: Duration::from_secs(5),
        stream_idle_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn real_http_sse_maps_request_and_emits_text_usage_then_completion() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    adapter
        .execute(
            &model_request(),
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    let request = captured.await.unwrap();
    server.await.unwrap();

    assert!(
        request
            .head
            .starts_with("POST /v1/chat/completions HTTP/1.1")
    );
    assert!(
        request
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer tenant-secret-token")
    );
    assert_eq!(request.body["model"], "local-agent-model");
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["stream_options"]["include_usage"], true);
    assert_eq!(request.body["max_tokens"], 64);
    assert_eq!(request.body["messages"][0]["role"], "user");
    assert_eq!(request.body["messages"][0]["content"], "say hello");
    assert_eq!(request.body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta { text: "Hel".into() },
            ModelStreamEvent::TextDelta { text: "lo".into() },
            ModelStreamEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
                cost_micros: 18,
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        ]
    );
}

#[tokio::test]
async fn cancellation_stops_a_live_provider_http_stream_without_waiting_for_timeout() {
    let first_delta = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"started\"},\"finish_reason\":null}]}\n\n";
    let (endpoint, _captured, server) = spawn_hanging_server(first_delta).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    let (events_tx, mut events_rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        adapter
            .execute(
                &model_request(),
                &credential,
                cancellation_for_task,
                events_tx,
            )
            .await
    });

    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "started".into()
        })
    );
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("cancelled provider request must stop promptly")
        .unwrap();
    server.abort();

    assert_eq!(result, Err(ProviderExecutionError::Cancelled));
}

#[tokio::test]
async fn idle_provider_stream_is_classified_as_a_retryable_timeout() {
    let first_delta = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"started\"},\"finish_reason\":null}]}\n\n";
    let (endpoint, _captured, server) = spawn_hanging_server(first_delta).await;
    let mut adapter_config = config(endpoint);
    adapter_config.stream_idle_timeout = Duration::from_millis(50);
    let adapter = OpenAiCompatibleAdapter::new(adapter_config).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    let task = tokio::spawn(async move {
        adapter
            .execute(
                &model_request(),
                &credential,
                CancellationToken::new(),
                events_tx,
            )
            .await
    });
    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "started".into()
        })
    );
    let result = tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("idle provider stream must stop promptly")
        .unwrap();
    server.abort();

    assert_eq!(
        result,
        Err(ProviderExecutionError::Provider {
            kind: ModelErrorKind::Timeout,
            retryable: true,
            status: None,
            message: "provider stream was idle beyond the configured timeout".into(),
        })
    );
}

#[tokio::test]
async fn done_without_finish_reason_is_not_reported_as_success() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, _captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    let error = adapter
        .execute(
            &model_request(),
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap_err();
    server.await.unwrap();

    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "partial".into()
        })
    );
    assert_eq!(events_rx.recv().await, None);
    assert_eq!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::Protocol,
            retryable: false,
            status: None,
            message: "provider stream completed without finish_reason".into(),
        }
    );
}

#[tokio::test]
async fn http_429_is_classified_without_leaking_the_bearer_token() {
    let (endpoint, _captured, server) = spawn_complete_server(
        429,
        r#"{"error":{"message":"token do-not-log-this-token must slow down"}}"#,
    )
    .await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("do-not-log-this-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(1);

    let error = adapter
        .execute(
            &model_request(),
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap_err();
    server.await.unwrap();

    assert!(matches!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::RateLimited,
            retryable: true,
            status: Some(429),
            ..
        }
    ));
    assert!(!format!("{error:?}").contains("do-not-log-this-token"));
}

#[tokio::test]
async fn fragmented_streamed_tool_call_is_assembled_before_completion() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_42\",\"function\":{\"name\":\"read_\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"file\",\"arguments\":\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, _captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    adapter
        .execute(
            &model_request(),
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    server.await.unwrap();

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::ToolCall {
                id: "call_42".into(),
                name: "read_file".into(),
                arguments: json!({"path":"a.txt"}),
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        ]
    );
}

#[tokio::test]
async fn tool_call_and_result_history_is_preserved_for_the_next_model_turn() {
    let (endpoint, captured, server) = spawn_complete_server(
        200,
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
    )
    .await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);
    let mut request = model_request();
    request.messages.extend([
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                tool_call_id: "call_42".into(),
                name: "read_file".into(),
                arguments: json!({"path":"a.txt"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_call_id: "call_42".into(),
                content: json!({"text":"hello"}),
            }],
        },
    ]);

    adapter
        .execute(&request, &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap();
    let request = captured.await.unwrap();
    server.await.unwrap();

    assert_eq!(request.body["messages"][1]["role"], "assistant");
    assert_eq!(request.body["messages"][1]["content"], Value::Null);
    assert_eq!(
        request.body["messages"][1]["tool_calls"][0],
        json!({
            "id":"call_42",
            "type":"function",
            "function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}
        })
    );
    assert_eq!(request.body["messages"][2]["role"], "tool");
    assert_eq!(request.body["messages"][2]["tool_call_id"], "call_42");
    assert_eq!(
        request.body["messages"][2]["content"],
        "{\"text\":\"hello\"}"
    );
}

#[tokio::test]
async fn private_object_storage_image_is_rejected_before_provider_egress() {
    let adapter =
        OpenAiCompatibleAdapter::new(config("http://127.0.0.1:1/v1/chat/completions".into()))
            .unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(1);
    let mut request = model_request();
    request.messages[0].content = vec![ContentPart::Image {
        media_type: "image/png".into(),
        source: "s3://tenant-private/run/image.png".into(),
    }];

    let error = adapter
        .execute(&request, &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::CapabilityMismatch,
            retryable: false,
            status: None,
            message:
                "image source must be an HTTP(S) URL or data URL resolved by the model gateway"
                    .into(),
        }
    );
}

async fn spawn_complete_server(
    status: u16,
    response_body: &str,
) -> (
    String,
    oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = response_body.to_owned();
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        let reason = if status == 200 { "OK" } else { "Error" };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        captured_tx.send(captured).ok();
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        captured_rx,
        server,
    )
}

async fn spawn_hanging_server(
    first_delta: &str,
) -> (
    String,
    oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_delta = first_delta.to_owned();
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        captured_tx.send(captured).ok();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(first_delta.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        std::future::pending::<()>().await;
    });
    (
        format!("http://{address}/v1/chat/completions"),
        captured_rx,
        server,
    )
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before headers completed");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8(buffer[..header_end].to_vec()).unwrap();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .map(str::parse::<usize>)
        })
        .unwrap()
        .unwrap();
    while buffer.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before body completed");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&buffer[header_end..header_end + content_length]).unwrap();
    CapturedRequest { head, body }
}
