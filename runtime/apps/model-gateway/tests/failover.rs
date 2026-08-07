mod support;

use agent_model_gateway::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing, ProviderRoute, execute_with_safe_failover,
};
use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role,
};
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
        },
        response_timeout: Duration::from_secs(1),
        stream_idle_timeout: Duration::from_millis(75),
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
                text: "fallback".into()
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
            text: "committed".into()
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
