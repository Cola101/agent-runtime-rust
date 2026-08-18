use agent_protocol::{
    TOOL_APPROVAL_DECISION_REASON_MAX_CHARS, TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
    ToolApprovalDecision, ToolApprovalDecisionCommand, ToolApprovalDecisionValidationError,
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
        decision_reason: None,
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

/// A refusal may carry what the person said; nothing else may.
///
/// An approval with an explanation has nowhere to go -- the model is about to
/// be handed the Tool's real output -- so a command that carries one is a
/// caller that has misunderstood the shape, and saying so here is cheaper than
/// a sentence that silently never appears.
#[test]
fn only_a_refusal_carries_a_reason() {
    let mut refused = command();
    refused.decision = ToolApprovalDecision::Deny;
    refused.decision_reason = Some("这个文件不该读".into());
    assert_eq!(refused.validate(), Ok(()));

    let mut allowed = command();
    allowed.decision_reason = Some("这个文件不该读".into());
    assert_eq!(
        allowed.validate(),
        Err(ToolApprovalDecisionValidationError::ReasonWithoutRefusal),
    );
}

/// The reason rides into every later model turn as a Tool result, so it is
/// paid for again on each one. A sentence is what this is for.
#[test]
fn a_refusal_reason_is_bounded_and_not_blank() {
    let mut empty = command();
    empty.decision = ToolApprovalDecision::Deny;
    empty.decision_reason = Some("   ".into());
    assert_eq!(
        empty.validate(),
        Err(ToolApprovalDecisionValidationError::InvalidReason),
    );

    let mut long = command();
    long.decision = ToolApprovalDecision::Deny;
    long.decision_reason = Some("不".repeat(TOOL_APPROVAL_DECISION_REASON_MAX_CHARS + 1));
    assert_eq!(
        long.validate(),
        Err(ToolApprovalDecisionValidationError::InvalidReason),
    );

    // Counted in characters, not bytes: this is 512 characters and 1536 bytes,
    // and a byte bound would cut a Chinese sentence at a third of an English
    // one for no reason a person could see.
    let mut exact = command();
    exact.decision = ToolApprovalDecision::Deny;
    exact.decision_reason = Some("不".repeat(TOOL_APPROVAL_DECISION_REASON_MAX_CHARS));
    assert_eq!(exact.validate(), Ok(()));
}

/// A command recorded before this field existed still reads.
#[test]
fn a_decision_written_without_a_reason_still_decodes() {
    let mut without = serde_json::to_value(command()).expect("serializable");
    let object = without.as_object_mut().expect("an object");
    assert!(
        !object.contains_key("decision_reason"),
        "a command with no reason must not write the key at all",
    );
    object.remove("decision_reason");
    let decoded: ToolApprovalDecisionCommand =
        serde_json::from_value(without).expect("an older command still decodes");
    assert_eq!(decoded.decision_reason, None);
}
