mod support;

use agent_model_gateway::{
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ProviderAdapter, ProviderCredential,
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

fn config(endpoint: String) -> OpenAiResponsesConfig {
    OpenAiResponsesConfig {
        endpoint,
        model: "gpt-agent".into(),
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
                // This fixture sends no `output_index`, so the event does not
                // claim one. Absent is the honest answer, not zero.
                block: None,
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

/// The same defect as the chat-completions adapter had, in this one.
///
/// A zero-argument function call arrives with `arguments` as `""`, and parsing
/// that as JSON took the whole turn down over a perfectly well formed call.
/// Fixed there this round; the sibling was found by the same sweep and had no
/// test at all.
#[tokio::test]
async fn a_function_call_with_no_arguments_is_a_call_with_no_arguments() {
    let sse = concat!(
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_7\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\n"
    );
    let (endpoint, captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);

    let outcome = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await;
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    let _ = captured.await;
    server.await.unwrap();

    assert!(
        outcome.is_ok(),
        "an argument-free call is not a malformed one: {outcome:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ToolCall { name, arguments, .. }
                if name == "read_file" && *arguments == json!({}))),
        "no arguments is an empty object, not a failure: {events:?}",
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
            retry_after_ms: None,
            message: "provider stream ended without response.completed".into(),
        }
    );
}

#[tokio::test]
async fn refusal_is_a_typed_item_instead_of_visible_text_delta() {
    let sse = concat!(
        "event: response.refusal.delta\ndata: {\"type\":\"response.refusal.delta\",\"delta\":\"I cannot\"}\n\n",
        "event: response.refusal.done\ndata: {\"type\":\"response.refusal.done\",\"refusal\":\"I cannot help with that.\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
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
    server.await.unwrap();

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::Refusal {
                text: "I cannot help with that.".into(),
            },
            ModelStreamEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cost_micros: 0,
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        ]
    );
}

#[tokio::test]
async fn malformed_private_state_is_rejected_before_network_egress() {
    let adapter =
        OpenAiResponsesAdapter::new(config("http://127.0.0.1:1/v1/responses".into())).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let mut invalid = request();
    invalid.messages.insert(
        0,
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning {
                summary: Vec::new(),
                private_state: Some(agent_protocol::ProviderPrivateState {
                    provider_id: "openai-primary".into(),
                    protocol: "openai_responses".into(),
                    model: "gpt-agent".into(),
                    format: "openai.responses.reasoning.v1".into(),
                    data: String::new(),
                }),
            }],
        },
    );
    let (events_tx, _events_rx) = mpsc::channel(8);

    let error = ProviderAdapter::from(adapter)
        .execute(
            "openai-primary",
            &invalid,
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ProviderExecutionError::InvalidConfiguration(
            "provider-private model state is malformed".into()
        )
    );
}

#[tokio::test]
async fn preserves_reasoning_state_for_same_provider_and_omits_it_for_another() {
    let reasoning_sse = concat!(
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_42\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checked constraints.\"}],\"encrypted_content\":\"enc-opaque\"}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":8,\"output_tokens\":2}}}\n\n"
    );
    let (endpoint, _captured, server) =
        support::spawn_sse_server("/v1/responses", reasoning_sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);
    ProviderAdapter::from(adapter)
        .execute(
            "openai-primary",
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
        panic!("first event must retain reasoning state");
    };
    assert_eq!(summary, &["Checked constraints."]);
    assert_eq!(private_state.provider_id, "openai-primary");
    assert_eq!(private_state.protocol, "openai_responses");
    assert_eq!(private_state.model, "gpt-agent");
    assert_eq!(
        private_state.data,
        "{\"encrypted_content\":\"enc-opaque\",\"id\":\"rs_42\"}"
    );

    let replay_request = ModelRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning {
                summary: summary.clone(),
                private_state: Some(private_state.clone()),
            }],
        }],
        ..request()
    };
    let completed_sse = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n";
    let (endpoint, captured, server) =
        support::spawn_sse_server("/v1/responses", completed_sse).await;
    let same_adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);
    ProviderAdapter::from(same_adapter)
        .execute(
            "openai-primary",
            &replay_request,
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let captured = captured.await.unwrap();
    server.await.unwrap();
    assert_eq!(captured.body["input"][0]["type"], "reasoning");
    assert_eq!(captured.body["input"][0]["id"], "rs_42");
    assert_eq!(captured.body["input"][0]["encrypted_content"], "enc-opaque");

    let (endpoint, captured, server) =
        support::spawn_sse_server("/v1/responses", completed_sse).await;
    let other_adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(8);
    ProviderAdapter::from(other_adapter)
        .execute(
            "openai-secondary",
            &replay_request,
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await
        .unwrap();
    let captured = captured.await.unwrap();
    server.await.unwrap();
    assert!(!captured.body.to_string().contains("enc-opaque"));
    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::PrivateStateOmitted {
            origin_provider_id: "openai-primary".into(),
            target_provider_id: "openai-secondary".into(),
            format: "openai.responses.reasoning.v1".into(),
        })
    );
}

/// A turn the provider's filter stopped is not a turn that ran out of length.
///
/// `response.incomplete` carries `incomplete_details.reason`, and OpenAI puts
/// two different endings there: `max_output_tokens` and `content_filter`.
/// Reading neither and reporting `Length` for both made a blocked prompt end
/// as a *successful* short answer -- the kernel turns `Length` into
/// `run.succeeded` (`crates/kernel/src/lib.rs`), so the one failure only the
/// person themselves can act on was drawn as "这段回答没说完 -- 模型到了单轮
/// 长度上限", telling them to shorten a prompt that was never too long.
///
/// openclaw splits exactly this event and says why: "a content-filtered turn
/// is a provider error rather than a truncated answer"
/// (`packages/ai/src/providers/openai-responses-terminal-usage.ts:87-105`,
/// where `status === "incomplete" && incompleteReason === "content_filter"`
/// short-circuits to an error before the ordinary incomplete-status mapping).
/// opencode maps the Responses reason to its own `content-filter` finish
/// (`packages/llm/src/protocols/openai-responses.ts:527`) and then raises it
/// as a session error, noting these turns may have produced no visible output
/// at all (`packages/opencode/src/session/prompt.ts:1295-1307`).
#[tokio::test]
async fn an_incomplete_response_stopped_by_the_content_filter_is_not_a_length_cap() {
    let sse = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Sure, here\"}\n\n",
        "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"content_filter\"},\"usage\":{}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
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
    server.await.unwrap();

    assert_eq!(
        events.last(),
        Some(&ModelStreamEvent::Completed {
            reason: ModelFinishReason::ContentFilter,
        }),
        "a filtered turn must end as content_filter, not as a length cap: {events:?}"
    );
}

/// The other `incomplete_details.reason`, kept apart from the first.
///
/// `max_output_tokens` is the ending this branch was written for, and reading
/// the reason must not cost it: a reply that really did hit its per-turn
/// ceiling still ends `Length`, which the kernel reports as a succeeded Run
/// carrying the truncation.
#[tokio::test]
async fn an_incomplete_response_that_hit_its_output_ceiling_is_still_a_length_cap() {
    let sse = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Sure, here\"}\n\n",
        "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
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
    server.await.unwrap();

    assert_eq!(
        events.last(),
        Some(&ModelStreamEvent::Completed {
            reason: ModelFinishReason::Length,
        }),
        "a turn that hit its output ceiling must still end as length: {events:?}"
    );
}
