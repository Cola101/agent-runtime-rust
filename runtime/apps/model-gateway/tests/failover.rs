mod support;

use agent_model_gateway::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing, ProviderRoute, execute_with_frozen_failover, execute_with_safe_failover,
};
use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFailoverPolicySnapshot, ModelFinishReason,
    ModelRequest, ModelStreamEvent, ProviderPrivateState, ReasoningPolicy, Role,
};
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use support::{spawn_http_server, spawn_streaming_then_stall_server};

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "hello".into(),
            }],
        }],
        tools: vec![],
        output_schema: None,
        reasoning: ReasoningPolicy::Minimal,
        max_output_tokens: 64,
    }
}

fn route(id: &str, endpoint: String) -> ProviderRoute {
    let adapter = OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
        endpoint,
        model: "test-model".into(),
        pricing: ProviderPricing {
            input_million_tokens_micros: 0,
            output_million_tokens_micros: 0,
            cached_input_million_tokens_micros: None,
        },
        response_timeout: Duration::from_secs(1),
        stream_idle_timeout: Duration::from_millis(75),
        max_output_tokens: None,
        supports_reasoning_effort: false,
    })
    .unwrap();
    ProviderRoute::new(
        id,
        adapter,
        ProviderCredential::bearer(format!("secret-{id}")).unwrap(),
    )
}

#[tokio::test]
async fn rate_limit_before_output_falls_back_to_the_next_provider() {
    let (primary_endpoint, primary_request, primary_server) = spawn_http_server(
        "/v1/chat/completions",
        429,
        r#"{"error":{"message":"busy"}}"#,
    )
    .await;
    let response = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let (fallback_endpoint, fallback_request, fallback_server) =
        spawn_http_server("/v1/chat/completions", 200, response).await;
    let routes = vec![
        route("primary", primary_endpoint),
        route("fallback", fallback_endpoint),
    ];
    let (events_tx, mut events_rx) = mpsc::channel(16);

    let selected =
        execute_with_safe_failover(&routes, &request(), CancellationToken::new(), events_tx)
            .await
            .unwrap();
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }

    assert_eq!(selected.provider_id, "fallback");
    assert_eq!(selected.failed_provider_ids, vec!["primary"]);
    assert_eq!(
        events,
        vec![
            ModelStreamEvent::TextDelta {
                text: "fallback".into(),
                block: None
            },
            ModelStreamEvent::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cost_micros: 0
            },
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop
            },
        ]
    );
    assert!(
        primary_request
            .await
            .unwrap()
            .head
            .contains("secret-primary")
    );
    assert!(
        fallback_request
            .await
            .unwrap()
            .head
            .contains("secret-fallback")
    );
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn private_state_omission_is_audited_but_does_not_block_safe_fallback() {
    let (primary_endpoint, primary_request, primary_server) = spawn_http_server(
        "/v1/chat/completions",
        429,
        r#"{"error":{"message":"busy"}}"#,
    )
    .await;
    let response = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (fallback_endpoint, fallback_request, fallback_server) =
        spawn_http_server("/v1/chat/completions", 200, response).await;
    let routes = vec![
        route("compatible-primary", primary_endpoint),
        route("compatible-fallback", fallback_endpoint),
    ];
    let mut rich_request = request();
    rich_request.messages.insert(
        0,
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning {
                summary: vec!["Safe public summary.".into()],
                private_state: Some(ProviderPrivateState {
                    provider_id: "openai-origin".into(),
                    protocol: "openai_responses".into(),
                    model: "gpt-agent".into(),
                    format: "openai.responses.reasoning.v1".into(),
                    data: "opaque-must-not-cross".into(),
                }),
            }],
        },
    );
    let (events_tx, mut events_rx) = mpsc::channel(16);

    let selected =
        execute_with_safe_failover(&routes, &rich_request, CancellationToken::new(), events_tx)
            .await
            .unwrap();
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }

    assert_eq!(selected.provider_id, "compatible-fallback");
    assert_eq!(selected.failed_provider_ids, ["compatible-primary"]);
    assert!(matches!(
        events[0],
        ModelStreamEvent::PrivateStateOmitted { ref target_provider_id, .. }
            if target_provider_id == "compatible-primary"
    ));
    assert!(matches!(
        events[1],
        ModelStreamEvent::PrivateStateOmitted { ref target_provider_id, .. }
            if target_provider_id == "compatible-fallback"
    ));
    assert!(matches!(events[2], ModelStreamEvent::TextDelta { .. }));
    let primary = primary_request.await.unwrap();
    let fallback = fallback_request.await.unwrap();
    assert!(!primary.body.to_string().contains("opaque-must-not-cross"));
    assert!(!fallback.body.to_string().contains("opaque-must-not-cross"));
    primary_server.await.unwrap();
    fallback_server.await.unwrap();
}

#[tokio::test]
async fn the_frozen_run_policy_caps_provider_attempts_before_any_output() {
    let (primary_endpoint, primary_request, primary_server) = spawn_http_server(
        "/v1/chat/completions",
        429,
        r#"{"error":{"message":"busy"}}"#,
    )
    .await;
    let routes = vec![
        route("primary", primary_endpoint),
        route(
            "must-not-run",
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
    ];
    let policy = ModelFailoverPolicySnapshot {
        max_provider_attempts: 1,
        fallback_on: BTreeSet::from([ModelErrorKind::RateLimited]),
    };
    let (events_tx, _events_rx) = mpsc::channel(16);

    let error = execute_with_frozen_failover(
        &routes,
        &request(),
        &policy,
        CancellationToken::new(),
        events_tx,
    )
    .await
    .expect_err("a one-attempt Run must not inherit the gateway's wider default");

    assert!(matches!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::RateLimited,
            ..
        }
    ));
    assert!(
        primary_request
            .await
            .unwrap()
            .head
            .contains("secret-primary")
    );
    primary_server.await.unwrap();
}

#[tokio::test]
async fn authentication_failure_never_falls_back() {
    let (primary_endpoint, primary_request, primary_server) = spawn_http_server(
        "/v1/chat/completions",
        401,
        r#"{"error":{"message":"bad key"}}"#,
    )
    .await;
    let routes = vec![
        route("primary", primary_endpoint),
        route(
            "must-not-run",
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
    ];
    let (events_tx, _events_rx) = mpsc::channel(16);

    let error =
        execute_with_safe_failover(&routes, &request(), CancellationToken::new(), events_tx)
            .await
            .unwrap_err();

    assert!(matches!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::Authentication,
            ..
        }
    ));
    assert!(
        primary_request
            .await
            .unwrap()
            .head
            .contains("secret-primary")
    );
    primary_server.await.unwrap();
}

#[tokio::test]
async fn partial_output_then_timeout_never_falls_back() {
    let partial =
        "data: {\"choices\":[{\"delta\":{\"content\":\"committed\"},\"finish_reason\":null}]}\n\n";
    let (primary_endpoint, primary_request, primary_server) = spawn_streaming_then_stall_server(
        "/v1/chat/completions",
        partial,
        Duration::from_millis(250),
    )
    .await;
    let routes = vec![
        route("primary", primary_endpoint),
        route(
            "must-not-run",
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
    ];
    let (events_tx, mut events_rx) = mpsc::channel(16);

    let error =
        execute_with_safe_failover(&routes, &request(), CancellationToken::new(), events_tx)
            .await
            .unwrap_err();

    assert!(matches!(
        error,
        ProviderExecutionError::Provider {
            kind: ModelErrorKind::Timeout,
            ..
        }
    ));
    assert_eq!(
        events_rx.recv().await,
        Some(ModelStreamEvent::TextDelta {
            text: "committed".into(),
            block: None
        })
    );
    assert!(
        primary_request
            .await
            .unwrap()
            .head
            .contains("secret-primary")
    );
    primary_server.await.unwrap();
}

#[test]
fn provider_route_debug_never_exposes_the_credential() {
    let route = route("primary", "http://127.0.0.1:9/v1/chat/completions".into());
    let rendered = format!("{route:?}");

    assert!(rendered.contains("primary"));
    assert!(!rendered.contains("secret-primary"));
    assert_eq!(
        &[
            ModelErrorKind::RateLimited,
            ModelErrorKind::Timeout,
            ModelErrorKind::Unavailable,
        ],
        ProviderRoute::safe_fallback_kinds()
    );
}

/// The gateway refuses to fail over on a content filter, whatever the policy
/// snapshot says.
///
/// The whitelist is stated twice on purpose -- here and in
/// `RuntimeExecutionPolicySnapshot::is_bounded_and_safe` -- because these are
/// two different doors into the same behaviour and a Run that arrives through
/// one must not get semantics the other would have refused. What is being kept
/// out is not a wasted call: a refused prompt handed to a second vendor is
/// content the first vendor declined being disclosed to a vendor that had
/// never seen it, on the runtime's own initiative, to buy a retry that cannot
/// work. So a snapshot naming it is rejected outright rather than quietly
/// ignored: silently dropping a member would leave an operator believing they
/// had configured something.
#[tokio::test]
async fn a_policy_that_fails_over_on_a_content_filter_is_refused_outright() {
    let routes = vec![
        route("primary", "http://127.0.0.1:9/v1/chat/completions".into()),
        route("fallback", "http://127.0.0.1:9/v1/chat/completions".into()),
    ];
    let policy = ModelFailoverPolicySnapshot {
        max_provider_attempts: 2,
        fallback_on: BTreeSet::from([ModelErrorKind::RateLimited, ModelErrorKind::ContentFilter]),
    };
    let (events_tx, _events_rx) = mpsc::channel(16);

    let error = execute_with_frozen_failover(
        &routes,
        &request(),
        &policy,
        CancellationToken::new(),
        events_tx,
    )
    .await
    .expect_err("a content-filter fallback must not be honoured");

    assert!(
        matches!(
            &error,
            ProviderExecutionError::InvalidConfiguration(message)
                if message == "runtime model failover policy is invalid"
        ),
        "the policy must be rejected before any provider is called: {error:?}",
    );
}
