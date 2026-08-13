use agent_protocol::{
    MCP_INPUT_REQUIRED_SCHEMA_VERSION, MCP_INPUT_RESOLUTION_SCHEMA_VERSION, McpElicitationRequest,
    McpInputAction, McpInputRequired, McpInputResolutionCommand, McpInputResponse,
};
use chrono::{Duration, Utc};
use std::collections::BTreeMap;
use uuid::Uuid;

fn pending() -> McpInputRequired {
    McpInputRequired {
        schema_version: MCP_INPUT_REQUIRED_SCHEMA_VERSION,
        input_id: Uuid::now_v7(),
        server_id: Uuid::now_v7(),
        server_name: "billing".into(),
        tool_call_id: "call-1".into(),
        binding_digest: "a".repeat(64),
        round: 1,
        request_state: " opaque/\u{2603}/=?base64?literal?=\n".into(),
        requests: BTreeMap::from([(
            "confirmation".into(),
            McpElicitationRequest::Form {
                message: "Confirm the invoice amount".into(),
                requested_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"confirmed": {"type": "boolean"}},
                    "required": ["confirmed"]
                }),
                meta: Some(serde_json::json!({"source": "billing"})),
            },
        )]),
    }
}

#[test]
fn a_durable_mrtr_request_preserves_opaque_state_and_validates_bounded_forms() {
    let request = pending();
    assert_eq!(request.validate(), Ok(()));

    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["request_state"], request.request_state);
    assert_eq!(encoded["requests"]["confirmation"]["mode"], "form");

    let mut sensitive = request;
    sensitive.requests.insert(
        "credential".into(),
        McpElicitationRequest::Form {
            message: "Send a token".into(),
            requested_schema: serde_json::json!({
                "type": "object",
                "properties": {"api_token": {"type": "string"}}
            }),
            meta: None,
        },
    );
    assert!(sensitive.validate().is_err());
}

#[test]
fn an_mrtr_resolution_is_versioned_expiring_and_exactly_bound() {
    let pending = pending();
    let issued_at = Utc::now();
    let command = McpInputResolutionCommand {
        schema_version: MCP_INPUT_RESOLUTION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        input_id: pending.input_id,
        input_version: 1,
        binding_digest: pending.binding_digest.clone(),
        responses: BTreeMap::from([(
            "confirmation".into(),
            McpInputResponse {
                action: McpInputAction::Accept,
                content: Some(serde_json::json!({"confirmed": true})),
                meta: None,
            },
        )]),
        issued_at,
        expires_at: issued_at + Duration::minutes(5),
    };
    assert_eq!(command.validate_for(&pending), Ok(()));

    let mut missing = command.clone();
    missing.responses.clear();
    assert!(missing.validate_for(&pending).is_err());

    let mut stale = command;
    stale.input_version = 2;
    assert!(stale.validate_for(&pending).is_err());
}
