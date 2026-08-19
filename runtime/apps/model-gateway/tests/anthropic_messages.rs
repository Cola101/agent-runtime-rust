mod support;

use agent_model_gateway::{
    AnthropicMessagesAdapter, AnthropicMessagesConfig, ProviderAdapter, ProviderCredential,
    ProviderExecutionError, ProviderPricing,
};
use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role, ToolSpec,
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn config(endpoint: String) -> AnthropicMessagesConfig {
    AnthropicMessagesConfig {
        endpoint,
        model: "claude-agent".into(),
        anthropic_version: "2023-06-01".into(),
        pricing: ProviderPricing {
            input_million_tokens_micros: 1_000_000,
            output_million_tokens_micros: 2_000_000,
            cached_input_million_tokens_micros: None,
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
                    tool_call_id: "toolu_42".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"a.txt"}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_call_id: "toolu_42".into(),
                    content: json!({"text":"hello"}),
                }],
            },
        ],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        }],
        output_schema: None,
        reasoning: ReasoningPolicy::Minimal,
        max_output_tokens: 64,
    }
}

#[tokio::test]
async fn maps_messages_tool_history_and_streamed_tool_input() {
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_99\",\"name\":\"read_file\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"b.txt\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (endpoint, captured, server) = support::spawn_sse_server("/v1/messages", sse).await;
    let adapter = AnthropicMessagesAdapter::new(config(endpoint)).unwrap();
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

    let lower_head = captured.head.to_ascii_lowercase();
    assert!(captured.head.starts_with("POST /v1/messages HTTP/1.1"));
    assert!(lower_head.contains("x-api-key: tenant-secret-token"));
    assert!(lower_head.contains("anthropic-version: 2023-06-01"));
    assert!(!lower_head.contains("authorization:"));
    assert_eq!(captured.body["model"], "claude-agent");
    assert_eq!(captured.body["system"][0]["text"], "Work safely");
    assert_eq!(captured.body["messages"][0]["role"], "user");
    assert_eq!(
        captured.body["messages"][1]["content"][0]["type"],
        "tool_use"
    );
    assert_eq!(captured.body["messages"][2]["role"], "user");
    assert_eq!(
        captured.body["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(captured.body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta {
                text: "Hello".into(),
                // The block this adapter's own `content_block_start` named. It
                // reads that index to track partial tools and reasoning and
                // used to drop it for text, which is what left a log unable to
                // tell two answers from one cut in half.
                block: Some(0),
            },
            ModelStreamEvent::ToolCall {
                id: "toolu_99".into(),
                name: "read_file".into(),
                arguments: json!({"path":"b.txt"}),
            },
            ModelStreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 3,
                cost_micros: 16,
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        ]
    );
}

#[tokio::test]
async fn structured_output_is_rejected_before_network_egress() {
    let adapter =
        AnthropicMessagesAdapter::new(config("http://127.0.0.1:1/v1/messages".into())).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(1);
    let mut request = request();
    request.output_schema = Some(json!({"type":"object"}));

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
            retry_after_ms: None,
            message: "structured output is not supported by the Anthropic Messages adapter".into(),
        }
    );
}

#[tokio::test]
async fn eof_without_message_stop_is_a_protocol_failure() {
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/messages", sse).await;
    let adapter = AnthropicMessagesAdapter::new(config(endpoint)).unwrap();
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
            retry_after_ms: None,
            message: "provider stream ended without message_stop".into(),
        }
    );
}

#[tokio::test]
async fn thinking_is_private_continuation_state_and_never_visible_text() {
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private thought\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-opaque\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Visible answer\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/messages", sse).await;
    let adapter = AnthropicMessagesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);
    ProviderAdapter::from(adapter)
        .execute(
            "anthropic-primary",
            &request(),
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

    let ModelStreamEvent::Reasoning {
        summary,
        private_state: Some(private_state),
    } = &events[0]
    else {
        panic!("first event must retain Anthropic thinking state");
    };
    assert!(summary.is_empty());
    assert_eq!(private_state.provider_id, "anthropic-primary");
    assert_eq!(private_state.protocol, "anthropic_messages");
    assert_eq!(private_state.model, "claude-agent");
    assert!(private_state.data.contains("private thought"));
    assert!(private_state.data.contains("sig-opaque"));
    assert_eq!(
        events[1],
        ModelStreamEvent::TextDelta {
            text: "Visible answer".into(),
            // Block 1, not 0: block 0 of this answer is the thinking. That the
            // two are different blocks is precisely what the log could not say
            // before, and it is the distinction a reader needs most.
            block: Some(1)
        }
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        ModelStreamEvent::TextDelta { text, .. } if text.contains("private thought")
    )));

    let replay_request = ModelRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning {
                summary: Vec::new(),
                private_state: Some(private_state.clone()),
            }],
        }],
        ..request()
    };
    let completed_sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (endpoint, captured, server) =
        support::spawn_sse_server("/v1/messages", completed_sse).await;
    let replay_adapter = AnthropicMessagesAdapter::new(config(endpoint)).unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);
    ProviderAdapter::from(replay_adapter)
        .execute(
            "anthropic-primary",
            &replay_request,
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let captured = captured.await.unwrap();
    server.await.unwrap();
    assert_eq!(
        captured.body["messages"][0]["content"][0]["type"],
        "thinking"
    );
    assert_eq!(
        captured.body["messages"][0]["content"][0]["thinking"],
        "private thought"
    );
    assert_eq!(
        captured.body["messages"][0]["content"][0]["signature"],
        "sig-opaque"
    );
}
