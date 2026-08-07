use agent_protocol::{
    RUN_CANCELLATION_SCHEMA_VERSION, RunCancellationCommand, RunCancellationValidationError,
};
use chrono::{Duration, Utc};
use uuid::Uuid;

const CANCELLATION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-cancellation-requested.v2.example.json");

fn command() -> RunCancellationCommand {
    let issued_at = Utc::now();
    RunCancellationCommand {
        schema_version: RUN_CANCELLATION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
        issued_at,
        expires_at: issued_at + Duration::seconds(30),
        reason: "user_requested".into(),
    }
}

#[test]
fn v2_cancellation_rejects_a_missing_worker_incarnation() {
    let mut stale_target = command();
    stale_target.worker_incarnation_id = Uuid::nil();

    assert_eq!(
        stale_target.validate(),
        Err(RunCancellationValidationError::MissingWorkerIncarnation)
    );
}

#[test]
fn targeted_cancellation_has_a_bounded_validity_window() {
    assert!(command().validate().is_ok());

    let mut expired_at_issue = command();
    expired_at_issue.expires_at = expired_at_issue.issued_at;
    assert_eq!(
        expired_at_issue.validate(),
        Err(RunCancellationValidationError::InvalidValidityWindow)
    );
}

#[test]
fn cancellation_reason_must_be_explicit() {
    let mut blank = command();
    blank.reason = "  ".into();

    assert_eq!(
        blank.validate(),
        Err(RunCancellationValidationError::BlankReason)
    );
}

#[test]
fn published_contract_example_decodes_and_validates() {
    let decoded: RunCancellationCommand =
        serde_json::from_str(CANCELLATION_EXAMPLE).expect("example must decode");

    assert_eq!(decoded.schema_version, RUN_CANCELLATION_SCHEMA_VERSION);
    assert!(!decoded.worker_incarnation_id.is_nil());
    assert!(decoded.validate().is_ok());
}
