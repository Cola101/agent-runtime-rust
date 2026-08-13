use agent_protocol::{
    CheckpointSnapshot, ContentPart, Message, RUN_RECOVERY_SCHEMA_VERSION, Role,
    RunCheckpointPublished, RunExecutionCommand, RunRecoveryCommand, RunStatus,
    SubagentBudgetUsage, SubagentResultDelivery, SubagentResultOutcome, SubagentResultSource,
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
fn subagent_usage_is_digest_bound_while_legacy_zero_usage_receipts_still_verify() {
    let source = SubagentResultSource {
        tool_call_id: "call_usage".into(),
        delegation_id: Uuid::now_v7(),
        binding_digest: "c".repeat(64),
        child_run_id: Uuid::now_v7(),
        child_terminal_event_id: Uuid::now_v7(),
    };
    let outcome = SubagentResultOutcome {
        terminal_status: RunStatus::Succeeded,
        content: serde_json::json!({"text": "done"}),
        is_error: false,
    };
    let legacy = SubagentResultDelivery::new(source.clone(), outcome.clone());
    assert!(legacy.verify_digest());
    assert_eq!(legacy.usage, SubagentBudgetUsage::default());

    let mut metered = SubagentResultDelivery::new_with_usage(
        source,
        outcome,
        SubagentBudgetUsage {
            tokens: 321,
            cost_micros: 45_000,
        },
    );
    assert!(metered.verify_digest());
    metered.usage.tokens += 1;
    assert!(
        !metered.verify_digest(),
        "a result receipt must not be able to under-report child usage after signing"
    );
}

#[test]
fn typed_subagent_transcript_is_digest_bound_and_rejects_incomplete_tool_pairs() {
    let transcript = vec![
        Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "Inspect the evidence.".into(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: "I will read it first.".into(),
                },
                ContentPart::ToolCall {
                    tool_call_id: "call_read".into(),
                    name: "workspace.read_text".into(),
                    arguments: serde_json::json!({"path": "EVIDENCE.txt"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_call_id: "call_read".into(),
                content: serde_json::json!({"text": "durable evidence"}),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::Text {
                text: "The evidence is durable.".into(),
            }],
        },
    ];
    let mut result = SubagentResultDelivery::new_with_usage_and_transcript(
        SubagentResultSource {
            tool_call_id: "call_spawn".into(),
            delegation_id: Uuid::now_v7(),
            binding_digest: "d".repeat(64),
            child_run_id: Uuid::now_v7(),
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "The evidence is durable."}),
            is_error: false,
        },
        SubagentBudgetUsage {
            tokens: 42,
            cost_micros: 700,
        },
        transcript,
    );
    assert!(result.verify_digest());
    assert!(result.is_well_formed());

    let mut orphan_result = result.clone();
    orphan_result.transcript.remove(1);
    assert!(
        !orphan_result.is_well_formed(),
        "a Tool result without its preceding Assistant Tool call must never be replayed"
    );

    result.transcript.remove(2);
    assert!(
        !result.verify_digest(),
        "a receipt must bind the exact model-visible child transcript"
    );
    assert!(
        !result.is_well_formed(),
        "an Assistant Tool call without its Tool result must never be replayed"
    );
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
