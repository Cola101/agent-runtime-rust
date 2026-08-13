use agent_protocol::{
    TOOL_RECONCILIATION_SCHEMA_VERSION, ToolReconciliationCommand, ToolReconciliationDecision,
    ToolReconciliationValidationError,
};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

fn command(decision: ToolReconciliationDecision) -> ToolReconciliationCommand {
    ToolReconciliationCommand {
        schema_version: TOOL_RECONCILIATION_SCHEMA_VERSION,
        reconciliation_id: Uuid::now_v7(),
        version: 1,
        tenant_id: Uuid::now_v7(),
        source_run_id: Uuid::now_v7(),
        source_terminal_event_id: Uuid::now_v7(),
        tool_call_id: "call_publish".into(),
        binding_digest: "a".repeat(64),
        operator_id: "operator@example.test".into(),
        decision,
        continuation_input: Some("Continue from the reconciled Tool result.".into()),
        issued_at: Utc::now(),
    }
}

#[test]
fn final_reconciliation_requires_a_bounded_continuation_input() {
    let applied = command(ToolReconciliationDecision::Applied {
        content: json!({"release":"v1", "published":true}),
        is_error: false,
    });
    applied.validate().expect("valid applied reconciliation");

    let mut missing = applied.clone();
    missing.continuation_input = None;
    assert_eq!(
        missing.validate().unwrap_err(),
        ToolReconciliationValidationError::InvalidContinuationInput
    );
}

#[test]
fn unresolved_reconciliation_cannot_start_a_continuation() {
    let unresolved = command(ToolReconciliationDecision::Unresolved);
    assert_eq!(
        unresolved.validate().unwrap_err(),
        ToolReconciliationValidationError::InvalidContinuationInput
    );

    let mut valid = unresolved;
    valid.continuation_input = None;
    valid
        .validate()
        .expect("unresolved decision without continuation");
}

#[test]
fn applied_reconciliation_rejects_an_unbounded_tool_result() {
    let oversized = command(ToolReconciliationDecision::Applied {
        content: json!({"payload": "x".repeat(256 * 1024 + 1)}),
        is_error: false,
    });
    assert!(
        oversized.validate().is_err(),
        "operator-supplied Tool results must be bounded before persistence or model egress"
    );
}
