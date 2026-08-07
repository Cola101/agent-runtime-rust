mod support;

use agent_model_gateway::{
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing,
};
use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role, ToolSpec,
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn config(endpoint: String) -> OpenAiResponsesConfig {
    OpenAiResponsesConfig {
        endpoint,
        model: "gpt-agent".into(),
        pricing: ProviderPricing {
            input_million_tokens_micros: 1_000_000,
            output_million_tokens_micros: 2_000_000,
        },
        response_timeout: Duration::from_secs(5),
        stream_idle_timeout: Duration::from_secs(5),
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentPart::Text {
                    text: "Work safely".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "Read a.txt".into(),
                }],
            },
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
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        }],
        output_schema: Some(json!({"type":"object","required":["answer"]})),
        reasoning: ReasoningPolicy::Balanced,
        max_output_tokens: 64,
    }
}

#[tokio::test]
async fn maps_typed_items_and_requires_response_completed() {
    let sse = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_99\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.txt\\\"}\"}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n"
    );
    let (endpoint, captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    let captured = captured.await.unwrap();
    server.await.unwrap();

    assert!(captured.head.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        captured
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer tenant-secret-token")
    );
    assert_eq!(captured.body["model"], "gpt-agent");
    assert_eq!(captured.body["stream"], true);
    assert_eq!(captured.body["store"], false);
    assert_eq!(captured.body["max_output_tokens"], 64);
    assert_eq!(captured.body["reasoning"]["effort"], "medium");
    assert_eq!(captured.body["input"][0]["role"], "system");
    assert_eq!(
        captured.body["input"][1]["content"][0]["type"],
        "input_text"
    );
    assert_eq!(captured.body["input"][2]["type"], "function_call");
    assert_eq!(captured.body["input"][3]["type"], "function_call_output");
    assert_eq!(captured.body["tools"][0]["name"], "read_file");
    assert_eq!(captured.body["tools"][0]["strict"], true);
    assert_eq!(captured.body["text"]["format"]["type"], "json_schema");
    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta {
                text: "Hello".into(),
            },
            ModelStreamEvent::ToolCall {
                id: "call_99".into(),
                name: "read_file".into(),
                arguments: json!({"path":"b.txt"}),
            },
            ModelStreamEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
                cost_micros: 18,
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        ]
    );
}

#[tokio::test]
async fn eof_without_terminal_event_is_a_protocol_failure() {
    let sse = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);

    let error = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();
    server.await.unwrap();

    assert_eq!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::Protocol,
            retryable: false,
            status: None,
            message: "provider stream ended without response.completed".into(),
        }
    );
}
