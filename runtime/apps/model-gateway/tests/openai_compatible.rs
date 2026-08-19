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
            ModelStreamEvent::TextDelta {
                text: "Hel".into(),
                block: Some(0),
            },
            ModelStreamEvent::TextDelta {
                text: "lo".into(),
                block: Some(0),
            },
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

/// A reasoning model streams its thinking, and this adapter dropped all of it.
///
/// The chunk shape is copied from a real server (a self-hosted vLLM serving
/// Qwen3): one short answer produced 34 `delta.reasoning` chunks and 2
/// `delta.content` chunks. Reading only `content` meant a person watched an
/// empty screen for the whole of the thinking and then saw four characters
/// appear -- and on a coding task that silence is most of the wall clock.
///
/// `reasoning_content` is the other spelling in the wild (DeepSeek's API and
/// the vLLM/SGLang reasoning parsers), so both are read.
#[tokio::test]
async fn streamed_reasoning_is_reported_rather_than_dropped() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"We need \"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"two characters.\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);

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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let thinking = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(
        thinking, "We need two characters.",
        "the model's thinking must reach the client: {events:?}",
    );
    // The answer is still the answer: reasoning is beside the content, not
    // instead of it.
    let answer = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(answer, "你好");
}

/// The other spelling, from the same family of servers.
#[tokio::test]
async fn reasoning_content_is_read_under_its_other_name() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);

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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::ReasoningDelta { text, .. } if text == "thinking"
        )),
        "`reasoning_content` is the same fact under another name: {events:?}",
    );
}

/// A provider that streams content as parts, not as a bare string.
///
/// `delta.content` is typed `string | null` by OpenAI, and several providers
/// send an array of parts instead (openclaw handles it and names Mistral's
/// thinking models: `openclaw/packages/ai/src/transports/openai-completions-transport.ts:1061-1101`).
/// `as_str()` returns None for an array, so the whole answer was skipped and
/// the Run finished `succeeded` with nothing in it -- the worst shape a
/// failure can take, because nothing anywhere says it happened.
#[tokio::test]
async fn content_streamed_as_parts_is_not_silently_dropped() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"Hel\"}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"lo\"}]},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let answer = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(
        answer, "Hello",
        "a part-shaped answer must still be the answer: {events:?}"
    );
}

/// A thinking part inside `delta.content` is thinking, not answer.
#[tokio::test]
async fn a_thinking_part_inside_content_is_reported_as_thinking() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"weighing it\"},{\"type\":\"text\",\"text\":\"done\"}]},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::ReasoningDelta { text, .. } if text == "weighing it")),
        "a thinking part must not be read as the answer: {events:?}",
    );
    let answer = events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(answer, "done");
}

/// A provider that repeats the tool call id and name on every fragment.
///
/// Both references replace these; we appended. Azure and some vLLM builds
/// resend the full id and name with each argument fragment, which turned one
/// call into `call_1call_1call_1` naming `read_fileread_file` -- a Tool that
/// does not exist, refused by the runtime, with the model told nothing useful.
#[tokio::test]
async fn a_repeated_tool_call_id_and_name_are_not_concatenated() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let call = events
        .iter()
        .find_map(|event| match event {
            ModelStreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => Some((id, name, arguments)),
            _ => None,
        })
        .expect("a tool call must be assembled");
    assert_eq!(call.0, "call_1", "an id resent is the same id: {events:?}");
    assert_eq!(
        call.1, "read_file",
        "a name resent is the same name: {events:?}"
    );
    assert_eq!(call.2["path"], "a.txt");
}

/// A refusal is words the model produced, and it arrives on its own field.
///
/// `content` is null when a model refuses; the text is in `refusal`. Reading
/// only content meant a refusal was reported as a successful, empty answer.
#[tokio::test]
async fn a_streamed_refusal_is_not_reported_as_an_empty_success() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":null,\"refusal\":\"I cannot help with that.\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
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
    let _ = captured.await.unwrap();
    server.await.unwrap();

    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::Refusal { text } if text == "I cannot help with that.")),
        "a refusal must reach the client rather than becoming an empty answer: {events:?}",
    );
}

/// A terminal word this build has never seen must not destroy a finished
/// answer.
///
/// We were the only one of the three that turned an unrecognised
/// `finish_reason` into a hard, non-retryable protocol error -- after the text
/// had already streamed to the person, and after the tokens had been paid for.
#[tokio::test]
async fn an_unknown_finish_reason_does_not_throw_away_the_answer() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answered\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"eos_token\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
    let outcome = adapter
        .execute(
            &model_request(),
            &credential,
            CancellationToken::new(),
            events_tx,
        )
        .await;
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }
    let _ = captured.await.unwrap();
    server.await.unwrap();

    assert!(
        outcome.is_ok(),
        "an unrecognised terminal word must not fail a finished answer: {outcome:?}",
    );
    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::TextDelta { text, .. } if text == "answered")),
        "the answer that already streamed must survive: {events:?}",
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::Completed { .. })),
        "the turn must still end: {events:?}",
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
            text: "started".into(),
            block: Some(0),
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
            text: "started".into(),
            block: Some(0),
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
            retry_after_ms: None,
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
            text: "partial".into(),
            block: Some(0),
        })
    );
    assert_eq!(events_rx.recv().await, None);
    assert_eq!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::Protocol,
            retryable: false,
            status: None,
            retry_after_ms: None,
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

/// Only the arguments are streamed in fragments.
///
/// This test used to send the *name* in two pieces (`read_` then `file`) and
/// assert they were concatenated. No reference implementation does that, and
/// nothing on the wire produces it: both read the identity fields as
/// last-value-wins and append only the arguments --
/// `opencode/packages/llm/src/protocols/utils/tool-stream.ts:125-132`
/// (`delta.id ?? current?.id`, `input: current.input + delta.text`) and
/// `openclaw/packages/ai/src/transports/openai-completions-transport.ts:771-778`
/// (`block.id = toolCall.id`, `block.name = ...`, `block.partialArgs += ...`).
///
/// The old premise was not merely unused: it forced the concatenating
/// assembly that turns a provider resending its id and name on every fragment
/// into a call named `read_fileread_file`.
#[tokio::test]
async fn fragmented_streamed_tool_call_is_assembled_before_completion() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_42\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
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
            retry_after_ms: None,
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
