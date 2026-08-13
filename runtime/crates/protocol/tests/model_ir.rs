use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelRequest, ModelStreamEvent, ProviderPrivateState,
    ReasoningPolicy, Role, ToolSpec,
};
use serde_json::json;

#[test]
fn model_request_captures_multimodal_tools_and_structured_output_without_provider_types() {
    let request = ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentPart::Text {
                    text: "inspect this".into(),
                },
                ContentPart::Image {
                    media_type: "image/png".into(),
                    source: "s3://tenant/run/image.png".into(),
                },
            ],
        }],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a workspace file".into(),
            input_schema: json!({"type": "object"}),
        }],
        output_schema: Some(json!({"type": "object", "required": ["summary"]})),
        reasoning: ReasoningPolicy::Balanced,
        max_output_tokens: 2048,
    };

    let encoded = serde_json::to_value(request).unwrap();

    assert_eq!(encoded["messages"][0]["content"][1]["type"], "image");
    assert_eq!(encoded["tools"][0]["name"], "read_file");
    assert_eq!(encoded["reasoning"], "balanced");
}

#[test]
fn provider_failures_have_policy_safe_categories() {
    let event = ModelStreamEvent::Failed {
        kind: ModelErrorKind::ContextOverflow,
        retryable: false,
        message: "context limit exceeded".into(),
    };

    let encoded = serde_json::to_value(event).unwrap();

    assert_eq!(encoded["type"], "failed");
    assert_eq!(encoded["kind"], "context_overflow");
    assert_eq!(encoded["retryable"], false);
}

#[test]
fn rich_model_items_separate_public_summary_from_provider_private_state() {
    let private_state = ProviderPrivateState {
        provider_id: "openai-primary".into(),
        protocol: "openai_responses".into(),
        model: "gpt-agent".into(),
        format: "openai.responses.reasoning.v1".into(),
        data: "opaque-encrypted-state".into(),
    };
    assert!(private_state.is_well_formed());

    let part = ContentPart::Reasoning {
        summary: vec!["Checked the constraints.".into()],
        private_state: Some(private_state.clone()),
    };
    let encoded = serde_json::to_value(&part).unwrap();
    assert_eq!(encoded["type"], "reasoning");
    assert_eq!(encoded["summary"][0], "Checked the constraints.");
    assert_eq!(encoded["private_state"]["provider_id"], "openai-primary");

    assert!(
        ModelStreamEvent::Reasoning {
            summary: vec!["Checked the constraints.".into()],
            private_state: Some(private_state),
        }
        .commits_provider_output()
    );
    assert!(
        ModelStreamEvent::Refusal {
            text: "I cannot help with that.".into(),
        }
        .commits_provider_output()
    );
}

#[test]
fn private_state_omission_is_a_non_committing_audit_event_without_opaque_data() {
    let event = ModelStreamEvent::PrivateStateOmitted {
        origin_provider_id: "openai-primary".into(),
        target_provider_id: "anthropic-fallback".into(),
        format: "openai.responses.reasoning.v1".into(),
    };

    assert!(!event.commits_provider_output());
    let encoded = serde_json::to_value(event).unwrap();
    assert_eq!(encoded["type"], "private_state_omitted");
    assert_eq!(encoded["origin_provider_id"], "openai-primary");
    assert_eq!(encoded["target_provider_id"], "anthropic-fallback");
    assert!(encoded.get("data").is_none());
}
