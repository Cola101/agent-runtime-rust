use agent_protocol::{
    CheckpointSnapshot, RUN_RECOVERY_SCHEMA_VERSION, RunCheckpointPublished, RunExecutionCommand,
    RunRecoveryCommand, RunStatus, SubagentResultDelivery, SubagentResultOutcome,
    SubagentResultSource,
};
use chrono::Utc;
use uuid::Uuid;

const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");

#[test]
fn recovery_v2_binds_one_durable_subagent_result_to_the_suspended_parent() {
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
        RunStatus::Suspended,
        12,
        br#"{"pending_subagent":true}"#.to_vec(),
    );
    let checkpoint = RunCheckpointPublished::new(
        &snapshot,
        execution.owner_epoch - 1,
        previous_fencing_token,
        "a".repeat(64),
        Utc::now(),
    );
    let result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: "call_spawn_reviewer_1".into(),
            delegation_id: Uuid::now_v7(),
            binding_digest: "b".repeat(64),
            child_run_id: Uuid::now_v7(),
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "The workspace evidence is consistent."}),
            is_error: false,
        },
    );
    let command = RunRecoveryCommand {
        schema_version: 2,
        message_id: Uuid::now_v7(),
        execution,
        checkpoint,
        subagent_result: Some(result.clone()),
        steering: None,
    };

    assert_eq!(command.schema_version, 2);
    assert!(RUN_RECOVERY_SCHEMA_VERSION > command.schema_version);
    assert!(result.verify_digest());
    assert!(command.validate().is_ok());
}

#[test]
fn legacy_recovery_cannot_smuggle_a_subagent_result() {
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
        RunStatus::Suspended,
        12,
        vec![1],
    );
    let checkpoint = RunCheckpointPublished::new(
        &snapshot,
        execution.owner_epoch - 1,
        previous_fencing_token,
        "a".repeat(64),
        Utc::now(),
    );
    let result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: "call_spawn_reviewer_1".into(),
            delegation_id: Uuid::now_v7(),
            binding_digest: "b".repeat(64),
            child_run_id: Uuid::now_v7(),
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "done"}),
            is_error: false,
        },
    );
    let command = RunRecoveryCommand {
        schema_version: 1,
        message_id: Uuid::now_v7(),
        execution,
        checkpoint,
        subagent_result: Some(result),
        steering: None,
    };

    assert_eq!(
        command.validate().unwrap_err().to_string(),
        "legacy recovery must not carry a subagent result"
    );
}
