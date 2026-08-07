use agent_protocol::{
    TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision, ToolApprovalDecisionCommand,
    ToolApprovalDecisionValidationError,
};
use chrono::{Duration, Utc};
use uuid::Uuid;

const EXAMPLE: &str =
    include_str!("../../../../contracts/events/tool-approval-decided.v2.example.json");

fn command() -> ToolApprovalDecisionCommand {
    let issued_at = Utc::now();
    ToolApprovalDecisionCommand {
        schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        approval_id: Uuid::now_v7(),
        approval_version: 2,
        binding_digest: "a".repeat(64),
        decision: ToolApprovalDecision::AllowOnce,
        issued_at,
        expires_at: issued_at + Duration::minutes(5),
    }
}

#[test]
fn v2_approval_decision_rejects_a_missing_worker_incarnation() {
    let mut stale_target = command();
    stale_target.worker_incarnation_id = Uuid::nil();

    assert_eq!(
        stale_target.validate(),
        Err(ToolApprovalDecisionValidationError::MissingWorkerIncarnation)
    );
}

#[test]
fn approval_decision_is_exactly_bound_and_short_lived() {
    assert!(command().validate().is_ok());

    let mut bad_digest = command();
    bad_digest.binding_digest = "A".repeat(64);
    assert_eq!(
        bad_digest.validate(),
        Err(ToolApprovalDecisionValidationError::InvalidBindingDigest)
    );

    let mut long_lived = command();
    long_lived.expires_at = long_lived.issued_at + Duration::seconds(301);
    assert_eq!(
        long_lived.validate(),
        Err(ToolApprovalDecisionValidationError::InvalidValidityWindow)
    );
}

#[test]
fn approval_decision_requires_the_post_decision_version() {
    let mut stale = command();
    stale.approval_version = 1;

    assert_eq!(
        stale.validate(),
        Err(ToolApprovalDecisionValidationError::InvalidApprovalVersion)
    );
}

#[test]
fn published_approval_decision_example_decodes_and_validates() {
    let decoded: ToolApprovalDecisionCommand =
        serde_json::from_str(EXAMPLE).expect("example must decode");

    assert_eq!(decoded.decision, ToolApprovalDecision::AllowOnce);
    assert_eq!(decoded.approval_version, 2);
    assert_eq!(
        decoded.schema_version,
        TOOL_APPROVAL_DECISION_SCHEMA_VERSION
    );
    assert!(!decoded.worker_incarnation_id.is_nil());
    assert!(decoded.validate().is_ok());
}
