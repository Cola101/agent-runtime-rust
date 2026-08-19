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
        max_output_tokens: None,
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
    // Forwarded unchanged, because this fixture configures no ceiling. What
    // bounds a reply is the operator's ceiling, not the adapter -- see
    // `without_a_ceiling_...` and `a_configured_ceiling_...` below.
    assert_eq!(request.body["max_tokens"], 64, "{}", request.body);
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

/// A server that answers `stream: true` with non-streaming-shaped chunks.
///
/// The choice carries a whole `message` where the streaming shape puts a
/// `delta`. Reading only `delta` found `Value::Null`, emitted nothing, and
/// then read `finish_reason` off the same choice and completed the turn --
/// so the Run succeeded with the answer nowhere in it, and nothing anywhere
/// said a word had been lost.
///
/// openclaw normalises this in both of its code paths
/// (`packages/ai/src/transports/openai-completions-transport.ts:672-674` and
/// `packages/ai/src/providers/openai-completions.ts:433-438`, whose comment
/// names the cause: "Some OpenAI-compatible endpoints deliver a full
/// `message` instead of `delta`").
#[tokio::test]
async fn a_message_shaped_chunk_still_carries_the_answer() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":\"stop\"}]}\n\n",
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
        "an answer sent in a message-shaped chunk is still the answer: {events:?}"
    );
}

/// The same shape, carrying a tool call and thinking rather than an answer.
///
/// Everything the choice holds hangs off the one field we did not read, so
/// the call the model asked for was dropped as silently as the text -- and
/// the turn still completed, reporting success for work that never happened.
#[tokio::test]
async fn a_message_shaped_chunk_still_asks_for_the_tool() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"reasoning\":\"which file\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_7\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
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
            ModelStreamEvent::ToolCall { id, name, arguments }
                if id == "call_7" && name == "read_file" && arguments["path"] == "a.txt")),
        "the call the model asked for must survive the chunk's shape: {events:?}",
    );
    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::ReasoningDelta { text, .. } if text == "which file")),
        "the thinking hangs off the same field and is lost the same way: {events:?}",
    );
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

/// A provider that reports a fault after it has already answered 200.
///
/// This is the primary error channel for the provider family this adapter
/// targets: vLLM, SGLang and the proxies in front of them accept the request,
/// start the stream, and report a rate limit or an overrun as a data frame.
/// Nothing here read `chunk["error"]`, so the frame was discarded and the Run
/// failed with "provider stream ended without [DONE]" -- a sentence about our
/// own framing, non-retryable, with the provider's actual words thrown away.
///
/// The pattern already exists one file over: `anthropic_messages.rs:244-263`
/// maps the in-band error type to a kind and redacts the message through the
/// credential. Codex does the same for its own envelope
/// (`codex-rs/codex-api/src/sse/responses.rs:387-421`).
#[tokio::test]
async fn a_mid_stream_error_frame_is_reported_as_what_it_says() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"error\":{\"type\":\"rate_limit_exceeded\",\"message\":\"rate limit reached, try again in 11s\"}}\n\n",
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

    let error = outcome.expect_err("a reported fault is a failure");
    let ProviderExecutionError::Provider {
        kind,
        retryable,
        message,
        ..
    } = &error
    else {
        panic!("expected a provider failure, got {error:?}");
    };
    assert_eq!(*kind, ModelErrorKind::RateLimited, "{error:?}");
    assert!(
        *retryable,
        "a rate limit is the textbook retryable failure: {error:?}"
    );
    assert!(
        message.contains("rate limit reached"),
        "the provider's own sentence is the only lead there is: {message}",
    );
    // And never the credential, whatever the provider echoed back.
    assert!(!format!("{error:?}").contains("tenant-secret-token"));
}

/// An explicit `"error": null` is not a fault.
///
/// Several servers put the key on every chunk. Treating its presence as the
/// signal would fail every stream they produce -- which is a worse failure
/// than the one being fixed, because it is total.
#[tokio::test]
async fn an_explicit_null_error_key_is_an_ordinary_chunk() {
    let sse = concat!(
        "data: {\"error\":null,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fine\"},\"finish_reason\":\"stop\"}]}\n\n",
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
        "a null error key must not fail the stream: {outcome:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::TextDelta { text, .. } if text == "fine")),
        "the answer must still arrive: {events:?}",
    );
}

/// A fault frame whose message is about the context window is not a rate
/// limit, and retrying it forever would be the wrong answer.
#[tokio::test]
async fn a_mid_stream_context_overrun_is_not_reported_as_retryable() {
    let sse = concat!(
        "data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"This model's maximum context length is 8192 tokens\"}}\n\n",
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
    while events_rx.recv().await.is_some() {}
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let error = outcome.expect_err("a reported fault is a failure");
    let ProviderExecutionError::Provider {
        kind, retryable, ..
    } = &error
    else {
        panic!("expected a provider failure, got {error:?}");
    };
    assert_eq!(*kind, ModelErrorKind::ContextOverflow, "{error:?}");
    assert!(
        !*retryable,
        "a prompt that does not fit will not fit next time: {error:?}"
    );
}

/// A stream that ends cleanly after saying it finished is finished.
///
/// `[DONE]` is an SSE framing convention, not the provider saying the turn
/// ended -- `finish_reason` is. Requiring the sentinel threw away a complete,
/// already-paid-for answer over a missing token: the text had streamed to the
/// person and the Run failed anyway, non-retryably.
///
/// opencode does not require it at all: `[DONE]` is filtered out with the
/// other keep-alives before the parser ever sees it
/// (`opencode/packages/llm/src/protocols/shared.ts:247`).
#[tokio::test]
async fn a_clean_end_after_finish_reason_is_a_finished_turn() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answered\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
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
        "a finished turn must not fail for want of [DONE]: {outcome:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event, ModelStreamEvent::TextDelta { text, .. } if text == "answered")),
        "the answer must survive: {events:?}",
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop
            }
        )),
        "the turn must end, and end as the provider said it did: {events:?}",
    );
}

/// A stream that stops before saying it finished is truncated, and still fails.
///
/// This is the half worth keeping. openclaw draws the same line and says why
/// in its own comment -- "[DONE] tracking distinguishes clean termination from
/// connection drops (EOF without [DONE] remains fail-closed)"
/// (`openclaw/packages/ai/src/transports/openai-completions-transport.ts:820-826`).
/// A connection dropped mid-answer looks exactly like a clean end except for
/// the missing `finish_reason`, and reporting it as success would hand the
/// model half a turn as though it were whole.
#[tokio::test]
async fn a_stream_cut_off_before_it_finished_is_still_a_failure() {
    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half a sen\"}}]}\n\n";
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
    while events_rx.recv().await.is_some() {}
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let error = outcome.expect_err("a truncated stream is not a finished turn");
    assert!(
        format!("{error:?}").contains("ended before"),
        "the failure must say the stream was cut off, not that a token was missing: {error:?}",
    );
}

/// A provider that asks for a tool and then says "stop".
///
/// Plenty do -- it is the commonest deviation in this family. Reported as
/// `Stop`, the consequence is not cosmetic: `requested_tool_turn`
/// (`runtime/apps/worker/src/lib.rs:10089-10094`) matches only
/// `Completed { reason: ToolCalls }`, so nothing plans the tool, nothing runs
/// it, and the kernel marks the Run **succeeded** with the call abandoned.
///
/// It also puts a Tool call with no result into the committed transcript,
/// which is the state a test of mine last round asserted was unreachable. It
/// was unreachable by the two paths that test drove; this is a third.
///
/// opencode promotes unconditionally --
/// `opencode/packages/llm/src/protocols/openai-chat.ts:465`:
/// `state.finishReason === "stop" && hasToolCalls ? "tool-calls" : ...`.
/// openclaw promotes only when there is no visible text and the stream ended
/// cleanly, and **drops the calls** otherwise
/// (`openclaw/.../openai-completions-transport.ts:815-834`). We take
/// opencode's form: emitting a call and then abandoning it is the one outcome
/// neither reference produces, and it is what we do today.
#[tokio::test]
async fn a_tool_call_that_closes_with_stop_still_asks_for_the_tool() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
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
        events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::ToolCall { .. })),
        "the call the model asked for must still be delivered: {events:?}",
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls
            }
        )),
        "a turn that asked for a tool ends as a tool turn, whatever word the \
         provider used: {events:?}",
    );
}

/// And a turn that asked for nothing still ends as a stop.
#[tokio::test]
async fn an_ordinary_answer_still_ends_as_a_stop() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"just words\"},\"finish_reason\":\"stop\"}]}\n\n",
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
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop
            }
        )),
        "promotion must need a tool call, not merely a stop: {events:?}",
    );
}

/// A tool that takes no arguments.
///
/// Models emit these with `arguments` as `""`, or omit the field entirely.
/// We parsed the accumulated string as JSON with no special case, so an empty
/// one failed to parse and took the whole turn down with a non-retryable
/// protocol error -- for a call that was perfectly well formed.
///
/// Both references treat empty as `{}`: opencode literally writes
/// `raw || "{}"` (`packages/llm/src/protocols/shared.ts:155-156`), and
/// openclaw returns `{}` for an empty or whitespace string
/// (`packages/ai/src/utils/json-parse.ts:130-132`).
#[tokio::test]
async fn a_tool_call_with_no_arguments_is_a_call_with_no_arguments() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_3\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
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
        "an argument-free call is not a malformed one: {outcome:?}"
    );
    let call = events
        .iter()
        .find_map(|event| match event {
            ModelStreamEvent::ToolCall {
                name, arguments, ..
            } => Some((name, arguments)),
            _ => None,
        })
        .expect("the call must be delivered");
    assert_eq!(call.0, "read_file");
    assert_eq!(
        *call.1,
        json!({}),
        "no arguments is an empty object, not a failure"
    );
}

/// Arguments that were cut off mid-object are still a failure, and the failure
/// says which call broke.
///
/// openclaw repairs and, failing that, silently substitutes `{}`
/// (`packages/ai/src/utils/json-parse.ts:134-145`). We do not agree: turning
/// `{"path": "/etc/pas` into `{}` runs a call the model never asked for, and
/// for a write or an exec that is not a smaller mistake than failing. opencode
/// errors too, and names the tool in the message
/// (`packages/llm/src/protocols/shared.ts:156`) -- which is the part we were
/// missing, because "invalid JSON" alone does not say which of eleven calls it
/// was.
#[tokio::test]
async fn truncated_tool_arguments_fail_and_name_the_call() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_4\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\": \\\"a\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
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
    while events_rx.recv().await.is_some() {}
    let _ = captured.await.unwrap();
    server.await.unwrap();

    let error = outcome.expect_err("half an argument object is not a call we can make");
    let said = format!("{error:?}");
    assert!(
        said.contains("write_file"),
        "the failure must name the call that broke: {said}",
    );
}

/// No reference does it: openclaw clamps a caller-supplied cap to the model's
/// own ceiling (`openai-completions-transport.ts:1834-1851`), opencode sends a
/// A configured ceiling is what makes a Run budget safe to send.
///
/// `request.max_output_tokens` carries the Run's remaining budget on an
/// ordinary turn -- 400,000 on the desktop -- and a real server rejects that
/// outright. The adapter cannot tell that intent apart from transcript
/// compaction's deliberate 256, so it does not try: it sends what it is given
/// and the operator's ceiling caps it.
///
/// This guard is the second half of `a_configured_ceiling_...` below: together
/// they pin that the ceiling, not the adapter's judgement, is what bounds a
/// reply.
#[tokio::test]
async fn without_a_ceiling_the_caller_s_number_goes_out_unchanged() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
    let adapter = OpenAiCompatibleAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
    let mut request = model_request();
    // What a desktop Run actually carries.
    request.max_output_tokens = 400_000;
    adapter
        .execute(&request, &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap();
    while events_rx.recv().await.is_some() {}
    let sent = captured.await.unwrap();
    server.await.unwrap();

    assert_eq!(
        sent.body["max_tokens"], 400_000,
        "with no ceiling the adapter must not invent one: {}",
        sent.body,
    );
}

/// When an operator does know their model's ceiling, it is sent -- and it is
/// the smaller of the two.
///
/// Asking for more than the Run can afford is paying for tokens the Run will
/// discard; asking for more than the model can produce is the failure this
/// whole change is about.
#[tokio::test]
async fn a_configured_ceiling_is_sent_and_never_exceeds_what_the_run_can_afford() {
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    for (ceiling, remaining, expected) in [(4_096_u64, 400_000_u64, 4_096_u64), (4_096, 100, 100)] {
        let (endpoint, captured, server) = spawn_complete_server(200, sse).await;
        let mut settings = config(endpoint);
        settings.max_output_tokens = Some(ceiling);
        let adapter = OpenAiCompatibleAdapter::new(settings).unwrap();
        let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
        let (events_tx, mut events_rx) = mpsc::channel(16);
        let mut request = model_request();
        request.max_output_tokens = remaining;
        adapter
            .execute(&request, &credential, CancellationToken::new(), events_tx)
            .await
            .unwrap();
        while events_rx.recv().await.is_some() {}
        let sent = captured.await.unwrap();
        server.await.unwrap();
        assert_eq!(
            sent.body["max_tokens"], expected,
            "ceiling {ceiling} against {remaining} remaining: {}",
            sent.body,
        );
    }
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
