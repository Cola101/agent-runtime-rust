use agent_protocol::{
    CheckpointSnapshot, RUN_RECOVERY_SCHEMA_VERSION, RunCheckpointPublished, RunExecutionCommand,
    RunRecoveryCommand, RunStatus, RunSteeringCommand, RunSteeringRequest, RunSteeringTarget,
};
use chrono::{Duration, Utc};
use uuid::Uuid;

const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");

#[test]
fn recovery_v3_applies_one_pending_steer_before_the_replacement_model_turn() {
    let mut execution: RunExecutionCommand = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    let previous_attempt_id = execution.attempt_id;
    let previous_fencing_token = execution.fencing_token;
    execution.attempt_id = Uuid::now_v7();
    execution.owner_epoch += 1;
    execution.fencing_token = Uuid::now_v7();
    let snapshot = CheckpointSnapshot::new(
        execution.run_id,
        execution.tenant_id,
        execution.session_id,
        previous_attempt_id,
        RunStatus::Running,
        4,
        br#"{"transcript":[]}"#.to_vec(),
    );
    let checkpoint = RunCheckpointPublished::new(
        &snapshot,
        execution.owner_epoch - 1,
        previous_fencing_token,
        "a".repeat(64),
        Utc::now(),
    );
    let issued_at = Utc::now();
    let steering = RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: execution.tenant_id,
            run_id: execution.run_id,
            attempt_id: execution.attempt_id,
            worker_id: execution.worker_id,
            worker_incarnation_id: execution.worker_incarnation_id,
        },
        RunSteeringRequest {
            input: "Use the authorization evidence and stop exploring UI code.".into(),
            issued_at,
            expires_at: issued_at + Duration::seconds(30),
        },
    );
    let recovery = RunRecoveryCommand {
        schema_version: RUN_RECOVERY_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        execution,
        checkpoint,
        subagent_result: None,
        steering: Some(steering),
    };

    assert_eq!(RUN_RECOVERY_SCHEMA_VERSION, 3);
    assert!(recovery.validate().is_ok());
}
