use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelRequest, ModelStreamEvent, ReasoningPolicy, Role,
    ToolSpec,
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
