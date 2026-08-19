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

/// An account with nothing left is a billing ending, even when the provider
/// says so after it has already answered 200.
///
/// `response.failed` reported `Protocol` for everything it carried, and
/// `Protocol` is neither retryable nor in any `fallback_on` set -- so an
/// exhausted key ended the Run then and there, told the person "回复的格式不对"
/// (nothing was malformed), and skipped the second candidate that exists for
/// exactly this case. `Billing` crosses to another provider even though it is
/// not retryable on the account that reported it (`failover.rs`,
/// `crosses_to_another_provider`).
///
/// codex splits this same event on the same field: `response.failed` reads
/// `error.code` and answers `insufficient_quota` with its own `QuotaExceeded`
/// rather than the generic stream error
/// (`codex-rs/codex-api/src/sse/responses.rs:387-400`, with
/// `is_quota_exceeded_error` at `:629-631`), and covers it end to end in
/// `codex-rs/core/tests/suite/quota_exceeded.rs`.
#[tokio::test]
async fn a_response_failed_carrying_an_exhausted_quota_is_a_billing_ending() {
    let sse = concat!(
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-1\",",
        "\"error\":{\"code\":\"insufficient_quota\",\"message\":\"You exceeded your current quota, please check your plan and billing details.\"}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);

    let error = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();
    server.await.unwrap();

    let ProviderExecutionError::Provider {
        kind, retryable, ..
    } = &error
    else {
        panic!("an in-band response.failed must be a provider error: {error:?}");
    };
    assert_eq!(
        (*kind, *retryable),
        (ModelErrorKind::Billing, false),
        "an exhausted quota reported on response.failed must be Billing, not Protocol: {error:?}",
    );
}

/// And the endings that are not about billing keep the one they had.
///
/// The split above reads the provider's own sentence, so it has to be the
/// provider's sentence and not the word "quota": a `response.failed` that says
/// nothing about an allowance is still an unnamed fault, and calling it
/// `Billing` would send a Run down the fallback chain on a guess.
#[tokio::test]
async fn a_response_failed_that_says_nothing_about_billing_is_still_a_protocol_ending() {
    let sse = concat!(
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-1\",",
        // Not `server_error`: that one is a transient fault and has its own
        // guard below. This sample is a code no build here has ever seen,
        // which is the case this arm is actually for.
        "\"error\":{\"code\":\"something_this_build_has_never_seen\",\"message\":\"the model produced an invalid response\"}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);

    let error = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();
    server.await.unwrap();

    let ProviderExecutionError::Provider {
        kind, retryable, ..
    } = &error
    else {
        panic!("an in-band response.failed must be a provider error: {error:?}");
    };
    assert_eq!(
        (*kind, *retryable),
        (ModelErrorKind::Protocol, false),
        "an unnamed response.failed must keep the ending it had: {error:?}",
    );
}

/// A prompt the provider's policy refused is a refusal, not a broken exchange.
///
/// The same event, the same field, a different answer. `Protocol` here said
/// "the provider sent something malformed", which points at the runtime and
/// invites a retry; what happened is that the *content* was declined, which
/// only the person who wrote it can do anything about. It is also the one kind
/// that must never be handed to a second provider: the request would be
/// re-sent, near-certainly refused again, and a vendor that had not seen the
/// content would now have it.
///
/// codex splits the same codes off the same event -- `invalid_prompt` and
/// `bio_policy` become `InvalidRequest`, `cyber_policy` gets its own error
/// (`codex-rs/codex-api/src/sse/responses.rs:387-410`). We agree that these
/// are not stream faults and disagree only about the destination: codex has no
/// content-filter category at this layer, so it files them as invalid
/// requests, while this runtime already ends a filtered *turn* as
/// `ContentFilter` (`ModelFinishReason`). Sending the error form of the same
/// refusal to the same word is what lets one sentence cover both.
#[tokio::test]
async fn a_response_failed_the_policy_refused_is_a_content_filter_ending() {
    let sse = concat!(
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-1\",",
        "\"error\":{\"code\":\"invalid_prompt\",\"message\":\"Your prompt was flagged as potentially violating our usage policy.\"}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, _events_rx) = mpsc::channel(8);

    let error = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .unwrap_err();
    server.await.unwrap();

    let ProviderExecutionError::Provider {
        kind, retryable, ..
    } = &error
    else {
        panic!("an in-band response.failed must be a provider error: {error:?}");
    };
    assert_eq!(
        (*kind, *retryable),
        (ModelErrorKind::ContentFilter, false),
        "a policy refusal must be a content filter, and must not be retried: {error:?}",
    );
}

/// The same fault must not end differently for arriving a moment later.
///
/// A transient server fault reaching us *before* the stream opens is
/// `HTTP 500` -> `(Unavailable, true)`: retried on this Provider and carried
/// to the next, because `Unavailable` is in the shipped `fallback_on`. The
/// identical fault arriving *after* the 200, as `response.failed` with
/// `code: "server_error"`, fell into the unnamed arm and became
/// `(Protocol, false)` -- no retry at all, and the second candidate never
/// called. Arrival timing is not a property of the failure.
///
/// codex splits this position five ways and defaults everything it cannot name
/// to `Retryable` (`codex-rs/codex-api/src/sse/responses.rs:387-410`). We do
/// not follow it that far: an unnamed code stays fatal here, because retrying a
/// fault this build cannot describe spends someone's money on a guess. What
/// changes is that the transient codes stop being unnamed.
#[tokio::test]
async fn a_response_failed_carrying_a_server_fault_is_retried_like_one() {
    let sse = concat!(
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-1\",",
        "\"error\":{\"code\":\"server_error\",\"message\":\"The server had an error while processing your request.\"}}}\n\n"
    );
    let (endpoint, _captured, server) = support::spawn_sse_server("/v1/responses", sse).await;
    let adapter = OpenAiResponsesAdapter::new(config(endpoint)).unwrap();
    let credential = ProviderCredential::bearer("tenant-secret-token").unwrap();
    let (events_tx, mut events_rx) = mpsc::channel(16);
    let failure = adapter
        .execute(&request(), &credential, CancellationToken::new(), events_tx)
        .await
        .expect_err("a failed response is a failure");
    while events_rx.recv().await.is_some() {}
    server.await.unwrap();

    match failure {
        ProviderExecutionError::Provider { kind, retryable, .. } => {
            assert_eq!(kind, ModelErrorKind::Unavailable, "{failure:?}");
            assert!(retryable, "a server fault is the definition of worth retrying");
        }
        other => panic!("expected a Provider failure, got {other:?}"),
    }
}
