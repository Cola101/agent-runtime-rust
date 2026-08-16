use agent_protocol::{
    ApprovalMode, BudgetDimension, ContentPart, McpServerSnapshot, Message, ModelFinishReason,
    ModelStreamEvent, ProviderPrivateState, RUN_CANCELLATION_SCHEMA_VERSION, Role,
    RunCancellationCommand, RunExecutionCommand, RunStatus, RunSteeringCommand, RunSteeringRequest,
    RunSteeringTarget, RuntimeExecutionPolicySnapshot, SandboxClass, SessionBranchSnapshot,
    SessionConversationTurn, SubagentBudgetUsage, SubagentResultDelivery, SubagentResultOutcome,
    SubagentResultSource, TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision,
    ToolApprovalDecisionCommand, ToolDescriptor, ToolEffect, WorkloadIdentityRenewalCommand,
};
use agent_runtime_worker::{
    SkillArtifactVerifier, WorkerAssignmentError, WorkerProcessor, WorkerRecoveryAction,
    WorkerToolDefinition, WorkloadIdentityRenewalOutcome, materialize_native_workspace,
    prepare_trusted_workspace_tool,
};
use agent_tool_runtime::ToolExecutionError;
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

const EXECUTION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v1.example.json");
const EXECUTION_V2_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v2.example.json");
const EXECUTION_V3_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v3.example.json");
const EXECUTION_V4_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v4.example.json");
const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");
const EXECUTION_V12_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v12.example.json");
const EXECUTION_V15_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v15.example.json");

fn signed_v6_child_command(signing_key: &SigningKey) -> RunExecutionCommand {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    let digest = value["skill_snapshots"][0]["artifact_digest"]
        .as_str()
        .unwrap();
    let signature = signing_key.sign(format!("agent-runtime-skill-v1.{digest}").as_bytes());
    value["skill_snapshots"][0]["signature"] =
        json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    let root_run_id = value["run_id"].as_str().unwrap().to_string();
    value["run_id"] = json!(Uuid::now_v7());
    value["lineage"] = json!({
        "root_run_id": root_run_id,
        "parent_run_id": root_run_id,
        "delegation_id": Uuid::now_v7(),
        "depth": 1,
        "role": "reviewer"
    });
    serde_json::from_value(value).unwrap()
}

fn register_workspace_read(worker: &mut WorkerProcessor) {
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "workspace.read_text".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::TrustedNative,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["tool:workspace.read".into()]),
            },
            description: "Read bounded workspace text".into(),
            input_schema: json!({"type":"object"}),
        })
        .unwrap();
}

fn started_tool_execution(
    effect: ToolEffect,
    tool_name: &str,
) -> (
    WorkerProcessor,
    RunExecutionCommand,
    agent_protocol::ToolExecutionRequest,
) {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: tool_name.into(),
            effect,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::Kata,
            implementation_digest: "b".repeat(64),
            required_scopes: BTreeSet::from(["workspace:write".into()]),
        },
        description: "Exercise one effect-aware Tool failure".into(),
        input_schema: json!({"type":"object"}),
    };
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.register_tool(definition).unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_effect_failure".into(),
                name: tool_name.into(),
                arguments: json!({}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(request) = planned.plan else {
        panic!("Tool must be executable");
    };
    worker
        .record_tool_execution_started(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    (worker, command, request)
}

#[test]
fn cancellation_after_a_non_replay_safe_tool_started_is_indeterminate_with_both_facts() {
    for effect in [ToolEffect::NonIdempotent, ToolEffect::Unknown] {
        let (mut worker, command, request) = started_tool_execution(effect, "publish");

        let terminal = worker.cancel(command.attempt_id).unwrap();

        assert_eq!(terminal.event_type, "run.indeterminate");
        assert_eq!(terminal.payload["tool_call_id"], request.call.id);
        assert_eq!(terminal.payload["effect"], json!(effect));
        assert_eq!(terminal.payload["replay_safe"], false);
        assert_eq!(terminal.payload["reason"], "tool_outcome_unknown");
        assert_eq!(terminal.payload["interrupted_by"], "cancellation");
        assert_eq!(terminal.payload["requested_status"], "cancelled");
        assert_eq!(
            worker.status(command.attempt_id).unwrap(),
            RunStatus::Indeterminate
        );
    }
}

#[test]
fn duration_timeout_after_a_non_replay_safe_tool_started_is_indeterminate_with_both_facts() {
    for effect in [ToolEffect::NonIdempotent, ToolEffect::Unknown] {
        let (mut worker, command, request) = started_tool_execution(effect, "publish");

        let terminal = worker.timeout_duration(command.attempt_id).unwrap();

        assert_eq!(terminal.event_type, "run.indeterminate");
        assert_eq!(terminal.payload["tool_call_id"], request.call.id);
        assert_eq!(terminal.payload["effect"], json!(effect));
        assert_eq!(terminal.payload["replay_safe"], false);
        assert_eq!(terminal.payload["reason"], "tool_outcome_unknown");
        assert_eq!(terminal.payload["interrupted_by"], "duration_timeout");
        assert_eq!(terminal.payload["requested_status"], "timed_out");
        assert_eq!(
            worker.status(command.attempt_id).unwrap(),
            RunStatus::Indeterminate
        );
    }
}

#[test]
fn replay_safe_or_not_started_work_keeps_the_requested_interruption_terminal() {
    for effect in [ToolEffect::Pure, ToolEffect::Idempotent] {
        let (mut worker, command, _) = started_tool_execution(effect, "safe_lookup");
        let terminal = worker.cancel(command.attempt_id).unwrap();
        assert_eq!(terminal.event_type, "run.cancelled");
        assert_eq!(
            worker.status(command.attempt_id).unwrap(),
            RunStatus::Cancelled
        );
    }

    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_duration_seconds = 1;
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    let terminal = worker.timeout_duration(command.attempt_id).unwrap();
    assert_eq!(terminal.event_type, "run.timed_out");
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        RunStatus::TimedOut
    );
}

#[test]
fn live_ambiguous_executor_failures_make_non_replay_safe_tools_indeterminate() {
    for effect in [ToolEffect::NonIdempotent, ToolEffect::Unknown] {
        let (mut worker, command, request) = started_tool_execution(effect, "publish");

        let events = worker
            .record_tool_execution_failure(
                command.attempt_id,
                request.call.id.clone(),
                &request.binding_digest,
                &ToolExecutionError::TimedOut,
            )
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "run.indeterminate");
        assert_eq!(events[0].payload["tool_call_id"], request.call.id);
        assert_eq!(events[0].payload["effect"], json!(effect));
        assert_eq!(events[0].payload["replay_safe"], false);
        assert_eq!(
            worker.status(command.attempt_id).unwrap(),
            RunStatus::Indeterminate
        );
    }
}

#[test]
fn replay_safe_executor_failures_return_a_redacted_tool_result() {
    for effect in [ToolEffect::Pure, ToolEffect::Idempotent] {
        let (mut worker, command, request) = started_tool_execution(effect, "safe_lookup");

        let events = worker
            .record_tool_execution_failure(
                command.attempt_id,
                request.call.id.clone(),
                &request.binding_digest,
                &ToolExecutionError::Engine("private engine detail".into()),
            )
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "tool.result");
        assert_eq!(events[0].payload["is_error"], true);
        assert_eq!(
            events[0].payload["content"]["error"]["code"],
            "tool_execution_failed"
        );
        assert!(
            !events[0]
                .payload
                .to_string()
                .contains("private engine detail")
        );
        assert_eq!(
            worker.status(command.attempt_id).unwrap(),
            RunStatus::Running
        );
    }
}

#[test]
fn proven_pre_side_effect_failure_remains_a_result_for_a_non_replay_safe_tool() {
    let (mut worker, command, request) =
        started_tool_execution(ToolEffect::NonIdempotent, "process.start");
    let session_id = Uuid::now_v7();

    let events = worker
        .record_tool_execution_failure(
            command.attempt_id,
            request.call.id.clone(),
            &request.binding_digest,
            &ToolExecutionError::ProcessSessionStartFailed {
                session_id,
                reason: "private spawn detail".into(),
            },
        )
        .unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "tool.result");
    assert_eq!(events[0].payload["is_error"], true);
    assert_eq!(
        events[0].payload["content"]["error"]["code"],
        "process_session_start_failed"
    );
    assert_eq!(
        events[0].payload["content"]["error"]["session_id"],
        session_id.to_string()
    );
    assert!(
        !events[0]
            .payload
            .to_string()
            .contains("private spawn detail")
    );
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        RunStatus::Running
    );
}

#[test]
fn root_session_history_is_model_context_but_never_pending_tool_work() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V15_EXAMPLE).unwrap();
    value["schema_version"] = json!(16);
    value["session_id"] = json!(Uuid::now_v7());
    value["history_import"] = serde_json::Value::Null;
    value["skill_snapshots"] = json!([]);
    value["mcp_servers"] = json!([]);
    value["delegated_scopes"] = json!([]);
    let historical_run = Uuid::now_v7();
    let historical_turn = SessionConversationTurn::new(
        1,
        historical_run,
        vec![
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "Read EVIDENCE.txt.".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall {
                    tool_call_id: "call_historical_read".into(),
                    name: "workspace.read_text".into(),
                    arguments: json!({"path": "EVIDENCE.txt"}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_call_id: "call_historical_read".into(),
                    content: json!({"text": "historical evidence"}),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::Text {
                    text: "I found the historical evidence.".into(),
                }],
            },
        ],
    );
    value["session_branch"] = serde_json::to_value(SessionBranchSnapshot::new(
        Uuid::now_v7(),
        2,
        vec![historical_turn],
    ))
    .unwrap();
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();

    worker.accept(command.clone(), command.issued_at).unwrap();
    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();

    assert_eq!(invocation.invocation.messages.len(), 6);
    assert_eq!(
        invocation.invocation.messages[1].role,
        agent_model_gateway_protocol::v1::ModelRole::User as i32
    );
    assert_eq!(
        invocation.invocation.messages[2].role,
        agent_model_gateway_protocol::v1::ModelRole::Assistant as i32
    );
    assert_eq!(
        invocation.invocation.messages[3].role,
        agent_model_gateway_protocol::v1::ModelRole::Tool as i32
    );
    assert!(matches!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::NoPendingToolCall)
    ));

    worker.start(command.attempt_id).unwrap();
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let mut stale_head = command.clone();
    stale_head.message_id = Uuid::now_v7();
    stale_head.attempt_id = Uuid::now_v7();
    stale_head.worker_id = Uuid::now_v7();
    stale_head.worker_incarnation_id = Uuid::now_v7();
    stale_head.owner_epoch += 1;
    stale_head.fencing_token = Uuid::now_v7();
    stale_head.issued_at = Utc::now();
    stale_head.lease_expires_at = stale_head.issued_at + Duration::minutes(5);
    stale_head.session_branch.as_mut().unwrap().generation += 1;
    let mut replacement = WorkerProcessor::new_with_incarnation(
        stale_head.worker_id,
        stale_head.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();

    assert!(matches!(
        replacement.restore(stale_head.clone(), checkpoint, stale_head.issued_at),
        Err(WorkerAssignmentError::CheckpointIdentityMismatch)
    ));
}

fn steering_command(command: &RunExecutionCommand, input: &str) -> RunSteeringCommand {
    let issued_at = command.issued_at + Duration::seconds(2);
    RunSteeringCommand::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        RunSteeringTarget {
            tenant_id: command.tenant_id,
            run_id: command.run_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
        },
        RunSteeringRequest {
            input: input.into(),
            issued_at,
            expires_at: issued_at + Duration::seconds(30),
        },
    )
}

#[test]
fn signed_skill_snapshot_injects_instructions_and_hides_unlisted_preinstalled_tools() {
    let signing_key = SigningKey::from_bytes(&[61; 32]);
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(5);
    let digest = "b1d6368bf33925654794f16dfb25622375778cd65a7e49c15cd169759300bb34";
    let signature = signing_key.sign(format!("agent-runtime-skill-v1.{digest}").as_bytes());
    value["skill_snapshots"] = json!([{
        "schema_version": 1,
        "application_id": "22222222-2222-4222-8222-222222222222",
        "skill_version_id": "0198a5a6-a7a8-7def-8abc-0123456789c0",
        "name": "workspace-review",
        "semantic_version": "1.0.0",
        "description": "Review workspace evidence",
        "instructions": "Read files before answering.",
        "tool_names": ["workspace.read_text"],
        "supported_platforms": ["darwin-arm64", "linux-arm64", "linux-x86_64"],
        "min_runtime_version": "0.1.0",
        "artifact_digest": digest,
        "signing_key_id": "local-skill-key",
        "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    for name in ["workspace.read_text", "workspace.search"] {
        worker
            .register_tool(WorkerToolDefinition {
                descriptor: ToolDescriptor {
                    name: name.into(),
                    effect: ToolEffect::Pure,
                    approval: ApprovalMode::Ask,
                    sandbox: SandboxClass::TrustedNative,
                    implementation_digest: "a".repeat(64),
                    required_scopes: BTreeSet::from(["tool:workspace.read".into()]),
                },
                description: format!("Trusted {name}"),
                input_schema: json!({"type":"object"}),
            })
            .unwrap();
    }
    worker.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "local-skill-key",
        signing_key.verifying_key(),
    ));

    worker.accept(command.clone(), command.issued_at).unwrap();
    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();

    assert_eq!(
        invocation
            .invocation
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["workspace.read_text"]
    );
    let agent_model_gateway_protocol::v1::content_part::Body::Text(system) =
        invocation.invocation.messages[0].content[0]
            .body
            .as_ref()
            .unwrap()
    else {
        panic!("system message must contain text");
    };
    assert!(
        system
            .text
            .contains("Review the workspace and explain evidence before conclusions.")
    );
    assert!(system.text.contains("[Skill workspace-review@1.0.0]"));
    assert!(system.text.contains("Read files before answering."));
}

#[test]
fn checkpoint_restore_rejects_a_valid_but_different_subagent_role() {
    let signing_key = SigningKey::from_bytes(&[62; 32]);
    let command = signed_v6_child_command(&signing_key);
    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    register_workspace_read(&mut original);
    original.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "local-skill-key",
        signing_key.verifying_key(),
    ));
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command.clone();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.lineage.role = "researcher".into();
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    register_workspace_read(&mut replacement);
    replacement.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "local-skill-key",
        signing_key.verifying_key(),
    ));

    assert_eq!(
        replacement.restore(
            replacement_command,
            checkpoint,
            command.issued_at + Duration::seconds(1),
        ),
        Err(WorkerAssignmentError::CheckpointIdentityMismatch)
    );
}

#[test]
fn immutable_model_policy_snapshot_is_forwarded_to_the_gateway_with_its_digest() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    let expected_snapshot = base64::engine::general_purpose::STANDARD
        .decode(&command.model_policy_snapshot_base64)
        .unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();

    let prepared = worker.prepare_model_invocation(command.attempt_id).unwrap();

    assert_eq!(prepared.invocation.schema_version, 3);
    assert_eq!(
        prepared.invocation.model_policy_snapshot_json,
        expected_snapshot
    );
    assert_eq!(
        prepared.invocation.model_policy_digest,
        command.model_policy_digest
    );
}

#[test]
fn newly_configured_workspace_is_materialized_beneath_uuid_scoped_native_roots() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().join("workspaces");
    std::fs::create_dir(&base).unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();

    let materialized = materialize_native_workspace(&base, tenant_id, workspace_id).unwrap();

    assert_eq!(
        materialized,
        std::fs::canonicalize(&base)
            .unwrap()
            .join(tenant_id.to_string())
            .join(workspace_id.to_string())
    );
    assert!(materialized.is_dir());
}

#[cfg(unix)]
#[test]
fn native_workspace_materialization_rejects_a_symlinked_tenant_boundary() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().join("workspaces");
    let outside = temporary.path().join("outside");
    std::fs::create_dir(&base).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let tenant_id = Uuid::now_v7();
    std::os::unix::fs::symlink(&outside, base.join(tenant_id.to_string())).unwrap();

    assert!(matches!(
        materialize_native_workspace(&base, tenant_id, Uuid::now_v7()),
        Err(WorkerAssignmentError::ToolExecutorConfiguration(_))
    ));
}

#[test]
fn immutable_agent_instructions_precede_the_user_turn_as_a_system_message() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V3_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();

    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();
    assert_eq!(invocation.invocation.messages.len(), 2);
    assert_eq!(
        invocation.invocation.messages[0].role,
        agent_model_gateway_protocol::v1::ModelRole::System as i32
    );
    assert_eq!(
        invocation.invocation.messages[1].role,
        agent_model_gateway_protocol::v1::ModelRole::User as i32
    );
    let system = invocation.invocation.messages[0].content[0]
        .body
        .as_ref()
        .and_then(|body| match body {
            agent_model_gateway_protocol::v1::content_part::Body::Text(text) => {
                Some(text.text.as_str())
            }
            _ => None,
        });
    assert_eq!(
        system,
        Some("Review the workspace and explain evidence before conclusions.")
    );
}

#[test]
fn built_in_subagent_spawn_suspends_and_checkpoints_the_exact_request() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn", "tool:workspace.read"]);
    value["subagent_roles"] = json!([{
        "name": "reviewer",
        "instructions": "Review evidence only.",
        "delegated_scopes": ["tool:workspace.read"]
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();
    assert_eq!(
        invocation
            .invocation
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "agent.spawn",
            "agent.wait",
            "agent.close",
            "agent.send",
            "agent.history",
            "agent.fork",
            "agent.rollback",
        ]
    );
    let send = invocation
        .invocation
        .tools
        .iter()
        .find(|tool| tool.name == "agent.send")
        .expect("agent.send definition");
    let send_schema: serde_json::Value =
        serde_json::from_slice(&send.input_schema_json).expect("agent.send schema");
    assert!(
        send_schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("idempotency_key"))),
        "the model-visible contract must require caller-stable message idempotency"
    );
    assert!(
        send_schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("generation"))),
        "new model calls must bind the handle generation they observed"
    );
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-review".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "reviewer",
                    "input": "Review the migration evidence.",
                    "max_tokens": 400,
                    "max_cost_cents": 30,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let request = planned
        .subagent_request
        .expect("agent.spawn must be a control-plane request, not a local process");
    assert_eq!(planned.event.event_type, "subagent.spawn.requested");
    assert_eq!(request.tool_call_id, "call-review");
    assert_eq!(request.role, "reviewer");
    assert_eq!(request.budget.max_tokens, 400);
    assert!(request.is_well_formed());
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        agent_protocol::RunStatus::Suspended
    );

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(state["pending_subagent"]["tool_call_id"], "call-review");
    assert_eq!(
        state["pending_subagent"]["binding_digest"],
        request.binding_digest
    );
    assert_eq!(worker.heartbeat(chrono::Utc::now()).active_runs, 0);
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        agent_protocol::RunStatus::Suspended
    );
}

#[test]
fn adjacent_subagents_cannot_reserve_more_than_the_parent_remaining_budget() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["budget"] = json!({
        "max_tokens": 700,
        "max_cost_cents": 50,
        "max_duration_seconds": 60
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    for (id, input) in [("call-budget-a", "Task A"), ("call-budget-b", "Task B")] {
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::ToolCall {
                    id: id.into(),
                    name: "agent.spawn".into(),
                    arguments: json!({
                        "role": "worker",
                        "input": input,
                        "max_tokens": 400,
                        "max_cost_cents": 30,
                        "max_duration_seconds": 20
                    }),
                },
            )
            .unwrap();
    }
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let first = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let first_request = first.subagent_request.unwrap();
    assert_eq!(first_request.tool_call_id, "call-budget-a");
    assert_eq!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::InvalidToolCall),
        "the second request is individually valid but exceeds the remaining unreserved budget"
    );

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(state["pending_subagents"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["pending_subagents"][0]["tool_call_id"],
        "call-budget-a"
    );
    assert_eq!(
        state["pending_tool_calls"][0]["id"], "call-budget-b",
        "failed admission must not consume the Tool call or emit a spawn intent"
    );
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        agent_protocol::RunStatus::Suspended
    );

    let first_result = SubagentResultDelivery::new_with_usage(
        SubagentResultSource {
            tool_call_id: first_request.tool_call_id,
            delegation_id: first_request.delegation_id,
            binding_digest: first_request.binding_digest,
            child_run_id: first_request.delegation_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Succeeded,
            content: json!({"text": "Task A complete"}),
            is_error: false,
        },
        SubagentBudgetUsage {
            tokens: 350,
            cost_micros: 200_000,
        },
    );
    let accepted = worker
        .record_subagent_result(command.attempt_id, &first_result)
        .unwrap();
    let duplicate = worker
        .record_subagent_result(command.attempt_id, &first_result)
        .unwrap();
    assert_eq!(
        duplicate, accepted,
        "replayed result delivery must return the original receipt"
    );
    assert_eq!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::InvalidToolCall),
        "actual child usage must remain charged after its reservation is released"
    );
    let settled = worker.checkpoint(command.attempt_id).unwrap();
    let settled: serde_json::Value = serde_json::from_slice(&settled.state).unwrap();
    assert_eq!(settled["budget_usage"]["tokens"], 350);
    assert_eq!(settled["budget_usage"]["cost_micros"], 200_000);
}

#[test]
fn every_async_handle_shares_one_recoverable_parent_budget_ledger() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["budget"] = json!({
        "max_tokens": 700,
        "max_cost_cents": 50,
        "max_duration_seconds": 30
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    let mut handles = Vec::new();
    for (call_id, input) in [
        ("call-ledger-a", "Create handle A."),
        ("call-ledger-b", "Create handle B."),
    ] {
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::ToolCall {
                    id: call_id.into(),
                    name: "agent.spawn".into(),
                    arguments: json!({
                        "role": "worker",
                        "input": input,
                        "mode": "async",
                        "max_tokens": 400,
                        "max_cost_cents": 30,
                        "max_duration_seconds": 20
                    }),
                },
            )
            .unwrap();
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::Completed {
                    reason: ModelFinishReason::ToolCalls,
                },
            )
            .unwrap();
        let spawn = worker
            .plan_next_tool_call(command.attempt_id)
            .unwrap()
            .subagent_request
            .unwrap();
        let agent_id = spawn.delegation_id;
        worker
            .record_subagent_spawned(command.attempt_id, &spawn)
            .unwrap();
        worker
            .record_async_subagent_result(
                command.attempt_id,
                agent_id,
                &SubagentResultDelivery::new(
                    SubagentResultSource {
                        tool_call_id: spawn.tool_call_id,
                        delegation_id: agent_id,
                        binding_digest: spawn.binding_digest,
                        child_run_id: agent_id,
                        child_terminal_event_id: Uuid::now_v7(),
                    },
                    SubagentResultOutcome {
                        terminal_status: agent_protocol::RunStatus::Succeeded,
                        content: json!({"text": "handle ready"}),
                        is_error: false,
                    },
                ),
            )
            .unwrap();
        handles.push(agent_id);
    }

    let first = worker
        .continue_async_subagent(command.attempt_id, handles[0], "ledger-a-1", "Run task A.")
        .unwrap()
        .active_request
        .unwrap();
    let second = worker
        .continue_async_subagent(command.attempt_id, handles[1], "ledger-b-1", "Run task B.")
        .unwrap()
        .active_request
        .unwrap();
    assert_eq!(first.budget.max_tokens, 400);
    assert_eq!(first.budget.max_cost_cents, 30);
    assert_eq!(first.budget.max_duration_seconds, 20);
    assert_eq!(
        second.budget.max_tokens, 300,
        "a second handle must not reserve the first handle's tokens again"
    );
    assert_eq!(second.budget.max_cost_cents, 20);
    assert_eq!(second.budget.max_duration_seconds, 10);
    assert!(
        matches!(
            worker.prepare_model_invocation(command.attempt_id),
            Err(WorkerAssignmentError::BudgetExhausted)
        ),
        "the parent model must not spend tokens already promised to children"
    );

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(
        state["subagent_budget_reservations"]
            .as_object()
            .map(serde_json::Map::len),
        Some(2)
    );

    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut tampered_state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    let removed = tampered_state["subagent_budget_reservations"]
        .as_object_mut()
        .unwrap()
        .remove(&first.delegation_id.to_string());
    assert!(removed.is_some());
    let tampered_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&tampered_state).unwrap(),
    );
    let mut tampered_replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    assert!(matches!(
        tampered_replacement.restore(
            replacement_command.clone(),
            tampered_checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        ),
        Err(WorkerAssignmentError::InvalidCheckpoint(_))
    ));
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    assert!(
        matches!(
            replacement.prepare_model_invocation(replacement_command.attempt_id),
            Err(WorkerAssignmentError::BudgetExhausted)
        ),
        "recovery must rebuild the same reservation fence"
    );

    replacement
        .record_async_subagent_result(
            replacement_command.attempt_id,
            handles[0],
            &SubagentResultDelivery::new_with_usage(
                SubagentResultSource {
                    tool_call_id: first.tool_call_id,
                    delegation_id: first.delegation_id,
                    binding_digest: first.binding_digest,
                    child_run_id: first.delegation_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "task A complete"}),
                    is_error: false,
                },
                SubagentBudgetUsage {
                    tokens: 100,
                    cost_micros: 100_000,
                },
            ),
        )
        .unwrap();
    assert_eq!(
        replacement
            .prepare_model_invocation(replacement_command.attempt_id)
            .unwrap()
            .invocation
            .max_output_tokens,
        300,
        "settlement must atomically replace the reservation with actual usage"
    );

    replacement
        .record_async_subagent_result(
            replacement_command.attempt_id,
            handles[1],
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: second.tool_call_id,
                    delegation_id: second.delegation_id,
                    binding_digest: second.binding_digest,
                    child_run_id: second.delegation_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "task B complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();
    assert_eq!(
        replacement
            .prepare_model_invocation(replacement_command.attempt_id)
            .unwrap()
            .invocation
            .max_output_tokens,
        600
    );

    let active = replacement
        .continue_async_subagent(
            replacement_command.attempt_id,
            handles[0],
            "ledger-close-active",
            "Start work that will finish before close.",
        )
        .unwrap()
        .active_request
        .unwrap();
    let queued = replacement
        .continue_async_subagent(
            replacement_command.attempt_id,
            handles[0],
            "ledger-close-queued",
            "Cancel this queued work when the handle closes.",
        )
        .unwrap();
    assert_eq!(
        queued.receipt.status,
        agent_runtime_worker::SubagentMessageStatus::Queued
    );
    replacement
        .record_async_subagent_result(
            replacement_command.attempt_id,
            handles[0],
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: active.tool_call_id,
                    delegation_id: active.delegation_id,
                    binding_digest: active.binding_digest,
                    child_run_id: active.delegation_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "active work complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();
    replacement
        .record_async_subagent_closed(replacement_command.attempt_id, handles[0])
        .unwrap();
    assert_eq!(
        replacement
            .prepare_model_invocation(replacement_command.attempt_id)
            .unwrap()
            .invocation
            .max_output_tokens,
        600,
        "closing a terminal handle must release every cancelled queued reservation"
    );
    let settled = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();
    let settled: serde_json::Value = serde_json::from_slice(&settled.state).unwrap();
    assert_eq!(
        settled["subagent_budget_reservations"]
            .as_object()
            .map(serde_json::Map::len),
        Some(0),
        "all terminal children must release their reservation"
    );
    assert_eq!(
        settled["subagent_message_receipts"][handles[0].to_string()]["ledger-close-queued"]["status"],
        "cancelled"
    );

    replacement
        .continue_async_subagent(
            replacement_command.attempt_id,
            handles[1],
            "ledger-parent-cancel",
            "This work is fenced by parent cancellation.",
        )
        .unwrap();
    let before_cancel = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();
    let before_cancel: serde_json::Value = serde_json::from_slice(&before_cancel.state).unwrap();
    assert_eq!(
        before_cancel["subagent_budget_reservations"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1)
    );
    replacement.cancel(replacement_command.attempt_id).unwrap();
    let cancelled = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();
    let cancelled: serde_json::Value = serde_json::from_slice(&cancelled.state).unwrap();
    assert_eq!(
        cancelled["subagent_budget_reservations"]
            .as_object()
            .map(serde_json::Map::len),
        Some(0),
        "parent cancellation must release every child reservation"
    );
}

#[test]
fn one_parent_cannot_admit_more_than_eight_concurrent_subagents() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["budget"] = json!({
        "max_tokens": 10_000,
        "max_cost_cents": 1_000,
        "max_duration_seconds": 300
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    for index in 0..9 {
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::ToolCall {
                    id: format!("call-{index}"),
                    name: "agent.spawn".into(),
                    arguments: json!({
                        "role": "worker",
                        "input": format!("Task {index}"),
                        "max_tokens": 10,
                        "max_cost_cents": 1,
                        "max_duration_seconds": 20
                    }),
                },
            )
            .unwrap();
    }
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    for index in 0..8 {
        let request = worker
            .plan_next_tool_call(command.attempt_id)
            .unwrap()
            .subagent_request
            .unwrap();
        assert_eq!(request.tool_call_id, format!("call-{index}"));
    }
    assert_eq!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::InvalidToolCall)
    );
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(state["pending_subagents"].as_array().unwrap().len(), 8);
    assert_eq!(state["pending_tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(state["pending_tool_calls"][0]["id"], "call-8");
}

#[test]
fn a_closed_async_subagent_handle_cannot_be_resurrected_by_send_after_recovery() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-spawn".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "worker",
                    "input": "Complete the first turn.",
                    "mode": "async",
                    "max_tokens": 100,
                    "max_cost_cents": 10,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let spawn = worker
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let agent_id = spawn.delegation_id;
    worker
        .record_subagent_spawned(command.attempt_id, &spawn)
        .unwrap();
    let result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: spawn.tool_call_id.clone(),
            delegation_id: agent_id,
            binding_digest: spawn.binding_digest.clone(),
            child_run_id: spawn.delegation_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Succeeded,
            content: json!({"text": "first turn complete"}),
            is_error: false,
        },
    );
    worker
        .record_async_subagent_result(command.attempt_id, agent_id, &result)
        .unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-close".into(),
                name: "agent.close".into(),
                arguments: json!({"agent_id": agent_id}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let close = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(close) = close.plan else {
        panic!("agent.close must be executed as an idempotent control Tool");
    };
    worker
        .record_tool_execution_started(command.attempt_id, &close.call.id, &close.binding_digest)
        .unwrap();
    worker
        .record_bound_tool_result(
            command.attempt_id,
            close.call.id,
            &close.binding_digest,
            json!({"agent_id": agent_id, "status": "succeeded", "already_terminal": true}),
            false,
        )
        .unwrap();
    worker
        .record_async_subagent_closed(command.attempt_id, agent_id)
        .unwrap()
        .expect("the first close must persist the irreversible lifecycle edge");

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    replacement
        .apply_model_event(
            replacement_command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-send-after-close".into(),
                name: "agent.send".into(),
                arguments: json!({
                    "agent_id": agent_id,
                    "generation": 1,
                    "message": "resurrect",
                    "idempotency_key": "resurrect-after-close"
                }),
            },
        )
        .unwrap();
    replacement
        .apply_model_event(
            replacement_command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    assert_eq!(
        replacement.plan_next_tool_call(replacement_command.attempt_id),
        Err(WorkerAssignmentError::InvalidToolCall),
        "agent.close must persist a terminal handle state that agent.send cannot resurrect"
    );
}

#[test]
fn a_completed_subagent_turn_can_be_planned_as_a_bounded_fork() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-fork-source".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "worker",
                    "input": "Complete the source turn.",
                    "mode": "async",
                    "max_tokens": 100,
                    "max_cost_cents": 10,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let spawn = worker
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let source_agent_id = spawn.delegation_id;
    worker
        .record_subagent_spawned(command.attempt_id, &spawn)
        .unwrap();
    worker
        .record_async_subagent_result(
            command.attempt_id,
            source_agent_id,
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: spawn.tool_call_id,
                    delegation_id: source_agent_id,
                    binding_digest: spawn.binding_digest,
                    child_run_id: source_agent_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "source turn complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-fork".into(),
                name: "agent.fork".into(),
                arguments: json!({
                    "source_agent_id": source_agent_id,
                    "source_generation": 1,
                    "through_activation_ordinal": 0,
                    "max_tokens": 80,
                    "max_cost_cents": 8,
                    "max_duration_seconds": 15
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let planned = worker
        .plan_next_tool_call(command.attempt_id)
        .expect("a valid completed source boundary must plan agent.fork");
    let agent_kernel::ToolPlan::Execute(request) = planned.plan else {
        panic!("agent.fork must be an idempotent Worker control Tool");
    };
    assert_eq!(request.call.name, "agent.fork");
    assert_eq!(request.effect, agent_protocol::ToolEffect::Idempotent);
    worker
        .record_tool_execution_started(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    let fork = worker
        .fork_async_subagent(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    assert!(fork.created);
    assert_ne!(fork.receipt.agent_id, source_agent_id);
    assert_eq!(fork.receipt.source_generation, 1);
    assert_eq!(fork.receipt.generation, 1);
    assert_eq!(fork.receipt.budget.max_tokens, 80);
    assert_eq!(fork.event.event_type, "subagent.forked");
    let source_before_recovery = worker
        .subagent_history(command.attempt_id, source_agent_id, None, 50)
        .unwrap();
    let fork_before_recovery = worker
        .subagent_history(command.attempt_id, fork.receipt.agent_id, None, 50)
        .unwrap();
    assert_eq!(source_before_recovery.turns.len(), 1);
    assert_eq!(fork_before_recovery.turns, source_before_recovery.turns);
    assert_eq!(fork_before_recovery.queued_messages, 0);
    assert_eq!(fork_before_recovery.status, "terminal");

    // Simulate a crash after fork provenance is checkpointed but before its
    // Tool result is appended. A replacement must reuse the same handle and
    // event rather than creating a second branch.
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    let replay = replacement
        .fork_async_subagent(
            replacement_command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.receipt, fork.receipt);
    assert_eq!(replay.event.event_id, fork.event.event_id);
    let source_after_recovery = replacement
        .subagent_history(replacement_command.attempt_id, source_agent_id, None, 50)
        .unwrap();
    let fork_after_recovery = replacement
        .subagent_history(
            replacement_command.attempt_id,
            fork.receipt.agent_id,
            None,
            50,
        )
        .unwrap();
    assert_eq!(source_after_recovery, source_before_recovery);
    assert_eq!(fork_after_recovery, fork_before_recovery);
}

#[test]
fn a_terminal_handle_can_plan_generation_bound_rollback_to_a_prior_turn() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-rollback-source".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "worker",
                    "input": "Complete generation one turn zero.",
                    "mode": "async",
                    "max_tokens": 100,
                    "max_cost_cents": 10,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let spawn = worker
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let agent_id = spawn.delegation_id;
    worker
        .record_subagent_spawned(command.attempt_id, &spawn)
        .unwrap();
    worker
        .record_async_subagent_result(
            command.attempt_id,
            agent_id,
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: spawn.tool_call_id,
                    delegation_id: agent_id,
                    binding_digest: spawn.binding_digest,
                    child_run_id: agent_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "generation one turn zero complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();

    let continuation = worker
        .continue_async_subagent(
            command.attempt_id,
            agent_id,
            "rollback-source-turn-one",
            "Complete generation one turn one.",
        )
        .unwrap();
    let second_request = continuation.active_request.unwrap();
    let second_result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: second_request.tool_call_id.clone(),
            delegation_id: second_request.delegation_id,
            binding_digest: second_request.binding_digest.clone(),
            child_run_id: second_request.delegation_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Succeeded,
            content: json!({"text": "generation one turn one complete"}),
            is_error: false,
        },
    );
    worker
        .record_async_subagent_result(command.attempt_id, agent_id, &second_result)
        .unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-rollback".into(),
                name: "agent.rollback".into(),
                arguments: json!({
                    "agent_id": agent_id,
                    "generation": 1,
                    "through_activation_ordinal": 0
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let planned = worker
        .plan_next_tool_call(command.attempt_id)
        .expect("a terminal handle must plan a generation-bound rollback");
    let agent_kernel::ToolPlan::Execute(request) = planned.plan else {
        panic!("agent.rollback must be an idempotent Worker control Tool");
    };
    assert_eq!(request.call.name, "agent.rollback");
    assert_eq!(request.effect, agent_protocol::ToolEffect::Idempotent);
    worker
        .record_tool_execution_started(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();

    let rollback = worker
        .rollback_async_subagent(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .expect("the planned rollback must create a new head generation");
    assert!(rollback.created);
    assert_eq!(rollback.receipt.agent_id, agent_id);
    assert_eq!(rollback.receipt.from_generation, 1);
    assert_eq!(rollback.receipt.generation, 2);
    assert_eq!(rollback.receipt.through_activation_ordinal, 0);
    assert_eq!(rollback.event.event_type, "subagent.rolled_back");

    let current_before_recovery = worker
        .subagent_history(command.attempt_id, agent_id, None, 50)
        .unwrap();
    let generation_one_before_recovery = worker
        .subagent_history_at_generation(command.attempt_id, agent_id, 1, None, 50)
        .expect("the superseded generation must remain readable");
    assert_eq!(current_before_recovery.generation, 2);
    assert_eq!(current_before_recovery.turns.len(), 1);
    assert_eq!(
        current_before_recovery.turns[0].result.content["text"],
        "generation one turn zero complete"
    );
    assert_eq!(generation_one_before_recovery.generation, 1);
    assert_eq!(generation_one_before_recovery.turns.len(), 2);
    assert_eq!(
        generation_one_before_recovery.turns[1].result.content["text"],
        "generation one turn one complete"
    );

    // Simulate a crash after the generation transition is checkpointed but
    // before the parent Tool result is recorded. Recovery must replay the same
    // transition rather than incrementing the handle again.
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut tampered_state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    tampered_state["subagent_archived_turns"][agent_id.to_string()]["1"]["result"]["content"]["text"] =
        json!("tampered archived result");
    let tampered_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&tampered_state).unwrap(),
    );
    let mut tampered_replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    assert!(matches!(
        tampered_replacement.restore(
            replacement_command.clone(),
            tampered_checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        ),
        Err(WorkerAssignmentError::InvalidCheckpoint(_))
    ));
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    let replay = replacement
        .rollback_async_subagent(
            replacement_command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.receipt, rollback.receipt);
    assert_eq!(replay.event.event_id, rollback.event.event_id);
    assert_eq!(
        replacement
            .subagent_history(replacement_command.attempt_id, agent_id, None, 50)
            .unwrap(),
        current_before_recovery
    );
    assert_eq!(
        replacement
            .subagent_history_at_generation(replacement_command.attempt_id, agent_id, 1, None, 50,)
            .unwrap(),
        generation_one_before_recovery
    );

    assert_eq!(
        replacement.continue_async_subagent_at_generation(
            replacement_command.attempt_id,
            agent_id,
            1,
            "stale-generation-one-command",
            "This stale command must not enter generation two.",
        ),
        Err(WorkerAssignmentError::InvalidToolCall),
        "a caller that observed the superseded generation must be fenced"
    );

    let generation_two = replacement
        .continue_async_subagent_at_generation(
            replacement_command.attempt_id,
            agent_id,
            2,
            "generation-two-turn",
            "Continue from the restored generation two head.",
        )
        .unwrap();
    let generation_two_request = generation_two.active_request.unwrap();
    assert_ne!(
        generation_two_request.binding_digest, second_request.binding_digest,
        "the new generation must fence the superseded continuation binding"
    );
    assert_eq!(
        replacement.record_async_subagent_result(
            replacement_command.attempt_id,
            agent_id,
            &second_result,
        ),
        Err(WorkerAssignmentError::SubagentResultBindingMismatch),
        "a late generation-one result must not settle generation two"
    );
    replacement
        .record_async_subagent_result(
            replacement_command.attempt_id,
            agent_id,
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: generation_two_request.tool_call_id,
                    delegation_id: generation_two_request.delegation_id,
                    binding_digest: generation_two_request.binding_digest,
                    child_run_id: generation_two_request.delegation_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "generation two turn complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();
    let generation_two_history = replacement
        .subagent_history(replacement_command.attempt_id, agent_id, None, 50)
        .unwrap();
    assert_eq!(generation_two_history.generation, 2);
    assert_eq!(
        generation_two_history
            .turns
            .iter()
            .map(|turn| turn.activation_ordinal)
            .collect::<Vec<_>>(),
        vec![0, 2],
        "activation ordinals are a monotonic audit sequence, not reused after rollback"
    );
    assert_eq!(
        replacement
            .subagent_history_at_generation(replacement_command.attempt_id, agent_id, 1, None, 50,)
            .unwrap(),
        generation_one_before_recovery,
        "continuing the new head must not mutate the archived generation"
    );
}

#[test]
fn async_subagent_message_receipt_replays_after_recovery_and_rejects_key_reuse() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-spawn-receipt".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "worker",
                    "input": "Complete the initial turn.",
                    "mode": "async",
                    "max_tokens": 100,
                    "max_cost_cents": 10,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let spawn = worker
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let agent_id = spawn.delegation_id;
    worker
        .record_subagent_spawned(command.attempt_id, &spawn)
        .unwrap();
    worker
        .record_async_subagent_result(
            command.attempt_id,
            agent_id,
            &SubagentResultDelivery::new(
                SubagentResultSource {
                    tool_call_id: spawn.tool_call_id,
                    delegation_id: agent_id,
                    binding_digest: spawn.binding_digest,
                    child_run_id: agent_id,
                    child_terminal_event_id: Uuid::now_v7(),
                },
                SubagentResultOutcome {
                    terminal_status: agent_protocol::RunStatus::Succeeded,
                    content: json!({"text": "initial turn complete"}),
                    is_error: false,
                },
            ),
        )
        .unwrap();

    let accepted = worker
        .continue_async_subagent(
            command.attempt_id,
            agent_id,
            "followup-message-1",
            "Inspect the durable follow-up.",
        )
        .unwrap();
    assert!(accepted.accepted_event.is_some());
    assert_eq!(accepted.receipt.message_sequence, 1);
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut legacy_state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    legacy_state["schema_version"] = json!(14);
    let legacy_receipt =
        &mut legacy_state["subagent_message_receipts"][agent_id.to_string()]["followup-message-1"];
    legacy_receipt.as_object_mut().unwrap().remove("status");
    legacy_receipt.as_object_mut().unwrap().remove("interrupt");
    legacy_state
        .as_object_mut()
        .unwrap()
        .remove("subagent_message_queues");
    legacy_state
        .as_object_mut()
        .unwrap()
        .remove("subagent_generations");
    legacy_state
        .as_object_mut()
        .unwrap()
        .remove("subagent_fork_receipts");
    legacy_state
        .as_object_mut()
        .unwrap()
        .remove("subagent_budget_reservations");
    let legacy_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&legacy_state).unwrap(),
    );
    let mut legacy_replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    legacy_replacement
        .restore(
            replacement_command.clone(),
            legacy_checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .expect("schema 14 message receipts must remain recoverable");
    let legacy_replay = legacy_replacement
        .continue_async_subagent(
            replacement_command.attempt_id,
            agent_id,
            "followup-message-1",
            "Inspect the durable follow-up.",
        )
        .unwrap();
    assert_eq!(
        legacy_replay.receipt.status,
        agent_runtime_worker::SubagentMessageStatus::Active
    );
    assert!(!legacy_replay.receipt.interrupt);
    let legacy_active = legacy_replay.active_request.unwrap();
    assert_eq!(
        Some(legacy_active.binding_digest.as_str()),
        accepted
            .active_request
            .as_ref()
            .map(|request| request.binding_digest.as_str())
    );
    assert!(
        legacy_active.conversation_history.is_empty(),
        "schema 14 cannot recover history it never stored"
    );

    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    let replay = replacement
        .continue_async_subagent(
            replacement_command.attempt_id,
            agent_id,
            "followup-message-1",
            "Inspect the durable follow-up.",
        )
        .unwrap();
    assert_eq!(replay.receipt, accepted.receipt);
    assert!(replay.accepted_event.is_none());
    assert_eq!(replay.active_request, accepted.active_request);
    assert_eq!(
        replacement.continue_async_subagent(
            replacement_command.attempt_id,
            agent_id,
            "followup-message-1",
            "Different content under the same key.",
        ),
        Err(WorkerAssignmentError::SubagentMessageConflict)
    );

    let checkpoint = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();
    let checkpoint: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(checkpoint["schema_version"], 26);
    assert_eq!(
        checkpoint["subagent_message_receipts"][agent_id.to_string()]
            .as_object()
            .map(serde_json::Map::len),
        Some(1),
        "replay must not create a second durable receipt"
    );
}

#[test]
fn interrupt_intent_is_checkpointed_before_an_active_subagent_is_stopped() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn"]);
    value["subagent_roles"] = json!([{
        "name": "worker",
        "instructions": "Solve one bounded task.",
        "delegated_scopes": []
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-spawn-before-interrupt".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "worker",
                    "input": "Continue until redirected.",
                    "mode": "async",
                    "max_tokens": 100,
                    "max_cost_cents": 10,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let spawn = worker
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let agent_id = spawn.delegation_id;
    worker
        .record_subagent_spawned(command.attempt_id, &spawn)
        .unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-interrupt-active-agent".into(),
                name: "agent.send".into(),
                arguments: json!({
                    "agent_id": agent_id,
                    "generation": 1,
                    "message": "Stop the old turn and do this now.",
                    "idempotency_key": "interrupt-active-1",
                    "interrupt": true
                }),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    worker
        .plan_next_tool_call(command.attempt_id)
        .expect("interrupting agent.send must be a valid Tool call");
    let continuation = worker
        .continue_async_subagent(
            command.attempt_id,
            agent_id,
            "interrupt-active-1",
            "Stop the old turn and do this now.",
        )
        .unwrap();
    assert_eq!(
        continuation.receipt.status,
        agent_runtime_worker::SubagentMessageStatus::Queued
    );

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let checkpoint_json: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    assert_eq!(
        checkpoint_json["subagent_message_receipts"][agent_id.to_string()]["interrupt-active-1"]["interrupt"],
        true,
        "a crash after accepting the message must retain the redirect intent"
    );
    assert_eq!(
        checkpoint_json["subagent_message_receipts"][agent_id.to_string()]["interrupt-active-1"]
            ["child_request"]["conversation_history"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "queued intent must not duplicate a stale history prefix before activation"
    );

    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut malformed_state = checkpoint_json.clone();
    malformed_state["subagent_message_receipts"][agent_id.to_string()]
        .as_object_mut()
        .unwrap()
        .remove("interrupt-active-1");
    let malformed_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&malformed_state).unwrap(),
    );
    let mut rejecting = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    assert!(matches!(
        rejecting.restore(
            replacement_command.clone(),
            malformed_checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        ),
        Err(WorkerAssignmentError::InvalidCheckpoint(_))
    ));

    let mut malformed_conversation_state = checkpoint_json.clone();
    malformed_conversation_state["subagent_activation_sequences"][agent_id.to_string()] = json!(99);
    let malformed_conversation_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&malformed_conversation_state).unwrap(),
    );
    let mut conversation_rejecting = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    assert!(matches!(
        conversation_rejecting.restore(
            replacement_command.clone(),
            malformed_conversation_checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        ),
        Err(WorkerAssignmentError::InvalidCheckpoint(_))
    ));

    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(
        replacement
            .pending_subagent_interrupts(replacement_command.attempt_id)
            .unwrap(),
        vec![agent_id],
        "replacement Worker must settle the redirect before relaunching ordinary work"
    );

    let interrupted = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: spawn.tool_call_id,
            delegation_id: agent_id,
            binding_digest: spawn.binding_digest,
            child_run_id: agent_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Cancelled,
            content: json!({"text": "interrupted before replacement"}),
            is_error: true,
        },
    );
    replacement
        .record_async_subagent_result(replacement_command.attempt_id, agent_id, &interrupted)
        .unwrap();
    let activation = replacement
        .activate_next_subagent_message(replacement_command.attempt_id, agent_id)
        .unwrap()
        .expect("durable redirect must activate after the old turn settles");
    assert!(activation.receipt.interrupt);
    assert_eq!(
        activation.request.input,
        "Stop the old turn and do this now."
    );
    assert_eq!(activation.request.conversation_history.len(), 1);
    assert_eq!(
        activation.request.conversation_history[0]
            .result
            .terminal_status,
        agent_protocol::RunStatus::Cancelled
    );
    let redirected = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: activation.request.tool_call_id.clone(),
            delegation_id: activation.request.delegation_id,
            binding_digest: activation.request.binding_digest.clone(),
            child_run_id: activation.request.delegation_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Succeeded,
            content: json!({"text": "replacement complete"}),
            is_error: false,
        },
    );
    replacement
        .record_async_subagent_result(replacement_command.attempt_id, agent_id, &redirected)
        .unwrap();
    let first_page = replacement
        .subagent_history(replacement_command.attempt_id, agent_id, None, 1)
        .unwrap();
    assert_eq!(first_page.turns[0].activation_ordinal, 0);
    assert_eq!(first_page.turns[0].message_sequence, 0);
    assert!(first_page.has_more);
    assert_eq!(first_page.next_after_activation_ordinal, Some(0));
    let second_page = replacement
        .subagent_history(replacement_command.attempt_id, agent_id, Some(0), 1)
        .unwrap();
    assert_eq!(second_page.turns[0].activation_ordinal, 1);
    assert_eq!(second_page.turns[0].message_sequence, 1);
    assert!(!second_page.has_more);
}

#[test]
fn restored_subagent_result_reenters_the_original_tool_call_and_resumes_model_work() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = json!(7);
    let run_id = value["run_id"].clone();
    value["lineage"] = json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    value["delegated_scopes"] = json!(["agent:spawn", "tool:workspace.read"]);
    value["subagent_roles"] = json!([{
        "name": "reviewer",
        "instructions": "Review evidence only.",
        "delegated_scopes": ["tool:workspace.read"]
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-review".into(),
                name: "agent.spawn".into(),
                arguments: json!({
                    "role": "reviewer",
                    "input": "Review the migration evidence.",
                    "max_tokens": 400,
                    "max_cost_cents": 30,
                    "max_duration_seconds": 20
                }),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let request = original
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .subagent_request
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    let result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: request.tool_call_id,
            delegation_id: request.delegation_id,
            binding_digest: request.binding_digest,
            child_run_id: Uuid::now_v7(),
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: agent_protocol::RunStatus::Succeeded,
            content: json!({"text": "Migration evidence is consistent."}),
            is_error: false,
        },
    );

    let event = replacement
        .record_subagent_result(replacement_command.attempt_id, &result)
        .unwrap();
    let invocation = replacement
        .prepare_model_invocation(replacement_command.attempt_id)
        .unwrap();

    assert_eq!(event.event_type, "subagent.result.received");
    assert_eq!(
        replacement.status(replacement_command.attempt_id).unwrap(),
        agent_protocol::RunStatus::Running
    );
    assert_eq!(
        replacement
            .recovery_action(replacement_command.attempt_id)
            .unwrap(),
        WorkerRecoveryAction::InvokeModel
    );
    let tool_result = invocation.invocation.messages.last().unwrap().content[0]
        .body
        .as_ref()
        .unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::ToolResult(tool_result) = tool_result
    else {
        panic!("subagent result must reenter the original tool call");
    };
    assert_eq!(tool_result.tool_call_id, "call-review");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&tool_result.content_json).unwrap(),
        json!({"text": "Migration evidence is consistent."})
    );
}

#[test]
fn signed_identity_renewal_extends_the_active_attempt_and_replaces_its_gateway_token() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    let signing_key = SigningKey::from_bytes(&[19; 32]);
    let issued_at = command.lease_expires_at - Duration::seconds(5);
    let lease_expires_at = command.lease_expires_at + Duration::seconds(30);
    let renewal = signed_renewal(&command, 2, issued_at, lease_expires_at, &signing_key);
    let expected_token = renewal.workload_token.clone();

    worker
        .apply_workload_identity_renewal(
            renewal,
            issued_at + Duration::seconds(1),
            &WorkloadTokenVerifier::new(signing_key.verifying_key()),
        )
        .unwrap();
    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();

    assert_eq!(invocation.workload_token, expected_token);
    assert_eq!(
        invocation.invocation.expires_at_unix_ms,
        lease_expires_at.timestamp_millis()
    );
}

#[test]
fn draining_worker_rejects_new_attempts_but_keeps_existing_attempts_renewable() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        2,
        "0.1.0".to_string(),
    )
    .unwrap();
    let accepted_at = command.issued_at + Duration::seconds(1);
    worker.accept(command.clone(), accepted_at).unwrap();
    let draining_since = accepted_at + Duration::seconds(1);
    let drain_deadline = draining_since + Duration::seconds(30);

    worker
        .begin_draining(draining_since, drain_deadline)
        .expect("valid drain window");

    let heartbeat = worker.heartbeat(draining_since + Duration::seconds(1));
    assert!(!heartbeat.accepting_work);
    assert_eq!(heartbeat.draining_since, Some(draining_since));
    assert_eq!(heartbeat.drain_deadline, Some(drain_deadline));
    assert_eq!(heartbeat.active_runs, 1);
    assert_eq!(
        heartbeat.active_assignments[0].attempt_id,
        command.attempt_id
    );

    // Redelivery of work already owned by this incarnation remains idempotent.
    assert_eq!(
        worker
            .accept(command.clone(), accepted_at)
            .unwrap()
            .attempt_id,
        command.attempt_id
    );

    let mut next = command;
    next.run_id = Uuid::now_v7();
    next.session_id = Uuid::now_v7();
    next.workspace_id = Uuid::now_v7();
    next.attempt_id = Uuid::now_v7();
    next.message_id = Uuid::now_v7();
    assert_eq!(
        worker.accept(next, accepted_at),
        Err(WorkerAssignmentError::Draining)
    );
}

#[test]
fn drain_window_is_one_way_and_must_be_positive() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let now = command.issued_at;

    assert_eq!(
        worker.begin_draining(now, now),
        Err(WorkerAssignmentError::InvalidDrainWindow)
    );
    worker
        .begin_draining(now, now + Duration::seconds(30))
        .unwrap();
    assert_eq!(
        worker.begin_draining(now + Duration::seconds(1), now + Duration::seconds(31)),
        Err(WorkerAssignmentError::AlreadyDraining)
    );
}

#[test]
fn process_wide_admission_fence_closes_without_cancelling_inflight_state_changes() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    let fence = worker.admission_fence();

    fence.close();

    assert!(!fence.is_open());
    assert_eq!(
        worker.accept(command.clone(), command.issued_at),
        Err(WorkerAssignmentError::Draining)
    );
    assert!(!worker.is_draining());
}

#[test]
fn stale_identity_renewal_cannot_roll_back_the_active_attempt() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    let signing_key = SigningKey::from_bytes(&[23; 32]);
    let first = signed_renewal(
        &command,
        2,
        command.lease_expires_at - Duration::seconds(5),
        command.lease_expires_at + Duration::seconds(30),
        &signing_key,
    );
    worker
        .apply_workload_identity_renewal(
            first,
            command.lease_expires_at - Duration::seconds(4),
            &WorkloadTokenVerifier::new(signing_key.verifying_key()),
        )
        .unwrap();
    let stale = signed_renewal(
        &command,
        1,
        command.lease_expires_at - Duration::seconds(3),
        command.lease_expires_at + Duration::seconds(60),
        &signing_key,
    );

    let error = worker
        .apply_workload_identity_renewal(
            stale,
            command.lease_expires_at - Duration::seconds(2),
            &WorkloadTokenVerifier::new(signing_key.verifying_key()),
        )
        .unwrap_err();

    assert_eq!(error, WorkerAssignmentError::StaleWorkloadIdentityRenewal);
}

#[test]
fn an_exact_identity_renewal_redelivery_is_idempotent_not_a_second_rotation() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    let signing_key = SigningKey::from_bytes(&[47; 32]);
    let received_at = command.lease_expires_at - Duration::seconds(4);
    let renewal = signed_renewal(
        &command,
        2,
        received_at - Duration::seconds(1),
        command.lease_expires_at + Duration::seconds(30),
        &signing_key,
    );
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    assert_eq!(
        worker
            .apply_workload_identity_renewal(renewal.clone(), received_at, &verifier)
            .unwrap(),
        WorkloadIdentityRenewalOutcome::Applied
    );
    assert_eq!(
        worker
            .apply_workload_identity_renewal(
                renewal,
                received_at + Duration::seconds(1),
                &verifier,
            )
            .unwrap(),
        WorkloadIdentityRenewalOutcome::Duplicate
    );
}

#[test]
fn identity_renewal_signed_by_an_untrusted_control_plane_is_rejected() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    let untrusted_key = SigningKey::from_bytes(&[29; 32]);
    let trusted_key = SigningKey::from_bytes(&[31; 32]);
    let renewal = signed_renewal(
        &command,
        2,
        command.lease_expires_at - Duration::seconds(5),
        command.lease_expires_at + Duration::seconds(30),
        &untrusted_key,
    );

    let error = worker
        .apply_workload_identity_renewal(
            renewal,
            command.lease_expires_at - Duration::seconds(4),
            &WorkloadTokenVerifier::new(trusted_key.verifying_key()),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        WorkerAssignmentError::InvalidWorkloadIdentityRenewal(_)
    ));
}

fn signed_renewal(
    command: &RunExecutionCommand,
    generation: u64,
    issued_at: chrono::DateTime<Utc>,
    lease_expires_at: chrono::DateTime<Utc>,
    signing_key: &SigningKey,
) -> WorkloadIdentityRenewalCommand {
    let claims = WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: command.tenant_id,
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: command.run_id,
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_incarnation_id,
        model_policy_id: command.model_policy_id,
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["checkpoint-gateway".into(), "model-gateway".into()]),
        scopes: BTreeSet::from([
            "checkpoint.read".into(),
            "checkpoint.write".into(),
            "model.execute".into(),
        ]),
        issued_at_unix_ms: issued_at.timestamp_millis(),
        expires_at_unix_ms: lease_expires_at.timestamp_millis(),
    };
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("v2.{encoded}");
    let token = format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(signing_key.sign(signing_input.as_bytes()).to_bytes())
    );
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "message_id": Uuid::now_v7(),
        "tenant_id": command.tenant_id,
        "run_id": command.run_id,
        "attempt_id": command.attempt_id,
        "worker_id": command.worker_id,
        "worker_incarnation_id": command.worker_incarnation_id,
        "owner_epoch": command.owner_epoch,
        "fencing_token": command.fencing_token,
        "generation": generation,
        "issued_at": issued_at,
        "lease_expires_at": lease_expires_at,
        "workload_token": token,
    }))
    .unwrap()
}

#[test]
fn targeted_execution_is_accepted_once_and_appears_in_heartbeat() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    let accepted_at = command.issued_at + Duration::seconds(1);

    let first = worker
        .accept(command.clone(), accepted_at)
        .expect("targeted command must be accepted");
    let duplicate = worker
        .accept(command.clone(), accepted_at + Duration::seconds(1))
        .expect("redelivery must be idempotent");
    let heartbeat = worker.heartbeat(accepted_at + Duration::seconds(2));

    assert_eq!(duplicate, first);
    assert_eq!(first.attempt_id, command.attempt_id);
    assert_eq!(first.worker_id, command.worker_id);
    assert_eq!(heartbeat.active_runs, 1);
    assert_eq!(heartbeat.active_assignments.len(), 1);
    assert_eq!(heartbeat.active_assignments[0].tenant_id, command.tenant_id);
    assert_eq!(heartbeat.active_assignments[0].run_id, command.run_id);
    assert_eq!(
        heartbeat.active_assignments[0].attempt_id,
        command.attempt_id
    );
    assert_eq!(
        heartbeat.active_assignments[0].owner_epoch,
        command.owner_epoch
    );
    assert_eq!(
        heartbeat.active_assignments[0].fencing_token,
        command.fencing_token
    );
    assert!(heartbeat.validate().is_ok());
}

#[test]
fn accepted_execution_enters_the_kernel_once() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .expect("targeted command must be accepted");

    let started = worker
        .start(command.attempt_id)
        .expect("accepted attempt must enter the kernel");
    let duplicate = worker
        .start(command.attempt_id)
        .expect("redelivery must reuse the original kernel start event");

    assert_eq!(started.event_type, "run.started");
    assert_eq!(started.sequence, 1);
    assert_eq!(started.run_id, command.run_id);
    assert_eq!(duplicate, started);
}

#[test]
fn terminal_model_turn_can_be_checkpointed_before_its_event_is_published() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::TextDelta {
                text: "terminal transcript evidence".into(),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        )
        .unwrap();

    let checkpoint = worker
        .checkpoint(command.attempt_id)
        .expect("terminal transcript must be durable before terminal publication");
    assert_eq!(checkpoint.status, agent_protocol::RunStatus::Succeeded);
    assert!(checkpoint.verify_digest());
    let transcript = WorkerProcessor::conversation_transcript_from_checkpoint(&checkpoint)
        .expect("terminal checkpoint carries a verified provider-neutral transcript");
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0].role, agent_protocol::Role::User);
    assert_eq!(
        transcript[0].content,
        vec![agent_protocol::ContentPart::Text {
            text: command.input,
        }]
    );
    assert_eq!(transcript[1].role, agent_protocol::Role::Assistant);
    assert_eq!(
        transcript[1].content,
        vec![agent_protocol::ContentPart::Text {
            text: "terminal transcript evidence".into(),
        }]
    );
}

#[test]
fn large_worker_checkpoint_is_prepared_as_a_verified_external_object() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Allow,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:read".into()]),
            },
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object"}),
        })
        .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_large".into(),
                name: "read_file".into(),
                arguments: json!({"path":"large.bin"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(execution) = planned.plan else {
        panic!("allow policy must execute");
    };
    worker
        .record_tool_execution_started(
            command.attempt_id,
            &execution.call.id,
            &execution.binding_digest,
        )
        .unwrap();
    let large_result = (0_u64..30_000)
        .map(|index| format!("{:x}", Sha256::digest(index.to_le_bytes())))
        .collect::<String>();
    worker
        .record_bound_tool_result(
            command.attempt_id,
            execution.call.id,
            &execution.binding_digest,
            json!({"content": large_result}),
            false,
        )
        .unwrap();

    let prepared = worker
        .prepare_checkpoint_message(command.attempt_id, Utc::now())
        .unwrap();
    let stored = prepared.external_payload.as_deref().unwrap();

    assert!(prepared.message.payload_base64.is_none());
    assert!(prepared.message.payload_ref.is_some());
    prepared
        .message
        .decode_snapshot_with_payload(stored)
        .unwrap();
}

#[test]
fn accepted_dispatch_builds_a_bound_first_turn_model_invocation() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .expect("command must be accepted first");

    let prepared = worker
        .prepare_model_invocation(command.attempt_id)
        .expect("accepted command must produce an invocation");

    assert_eq!(prepared.invocation.tenant_id, command.tenant_id.to_string());
    assert_eq!(prepared.invocation.run_id, command.run_id.to_string());
    assert_eq!(
        prepared.invocation.attempt_id,
        command.attempt_id.to_string()
    );
    assert_eq!(
        prepared.invocation.model_policy_id,
        command.model_policy_id.to_string()
    );
    assert_eq!(
        prepared.invocation.max_output_tokens,
        command.budget.max_tokens
    );
    assert_eq!(
        prepared.workload_token.as_str(),
        command.workload_token.as_str()
    );
    assert_eq!(
        format!("{:?}", prepared.workload_token),
        "WorkloadToken[REDACTED]"
    );
}

#[test]
fn v10_dispatch_carries_the_exact_runtime_policy_to_model_execution() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    command.schema_version = 10;
    command.skill_snapshots.clear();
    let mut runtime_policy = RuntimeExecutionPolicySnapshot {
        schema_version: 1,
        ..RuntimeExecutionPolicySnapshot::default()
    };
    runtime_policy.tool_execution.max_concurrent_tools = 1;
    runtime_policy.mcp_discovery.max_attempts_per_server = 1;
    runtime_policy.mcp_discovery.initial_retry_backoff_ms = 0;
    runtime_policy.model_failover.max_provider_attempts = 2;
    command.runtime_policy = Some(runtime_policy.clone());
    command.validate().unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();

    let invocation = worker
        .prepare_model_invocation(command.attempt_id)
        .unwrap()
        .invocation;
    let decoded: RuntimeExecutionPolicySnapshot =
        serde_json::from_slice(&invocation.runtime_policy_snapshot_json).unwrap();

    assert_eq!(invocation.schema_version, 4);
    assert_eq!(decoded, runtime_policy);
    assert_eq!(
        invocation.runtime_policy_digest,
        hex::encode(Sha256::digest(&invocation.runtime_policy_snapshot_json))
    );
}

/// The production break this catches is accepting a complete v20 invocation
/// and then downgrading its model request to the older tenant/Run-only wire
/// identity before it reaches the gateway.
#[test]
fn v20_dispatch_carries_the_complete_runtime_identity_to_model_execution() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V12_EXAMPLE).unwrap();
    command.schema_version = agent_protocol::RUN_EXECUTION_SCHEMA_VERSION;
    command.application_id = Uuid::now_v7();
    command.workload_identity_id = Uuid::now_v7();
    command.session_branch = Some(SessionBranchSnapshot::new(Uuid::now_v7(), 1, Vec::new()));
    command.skill_snapshots.clear();
    command.mcp_servers.clear();
    command.runtime_policy = Some(RuntimeExecutionPolicySnapshot::default());
    command.validate().unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();

    let invocation = worker
        .prepare_model_invocation(command.attempt_id)
        .unwrap()
        .invocation;

    assert_eq!(invocation.schema_version, 5);
    assert_eq!(
        invocation.application_id,
        command.application_id.to_string()
    );
    assert_eq!(
        invocation.workload_identity_id,
        command.workload_identity_id.to_string()
    );
    assert_eq!(invocation.session_id, command.session_id.to_string());
    assert_eq!(invocation.workspace_id, command.workspace_id.to_string());
    assert_eq!(
        invocation.agent_version_id,
        command.agent_version_id.to_string()
    );
}

/// The production break this catches is trusting the unsigned identity fields
/// of a v20 command before Worker admission. The token must bind the complete
/// invocation chain, not only the Run and Worker.
#[test]
fn v20_worker_admission_rejects_a_workspace_not_bound_by_the_workload_token() {
    let signing_key = SigningKey::from_bytes(&[64; 32]);
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V12_EXAMPLE).unwrap();
    command.schema_version = agent_protocol::RUN_EXECUTION_SCHEMA_VERSION;
    command.application_id = Uuid::now_v7();
    command.workload_identity_id = Uuid::now_v7();
    command.session_branch = Some(SessionBranchSnapshot::new(Uuid::now_v7(), 1, Vec::new()));
    command.skill_snapshots.clear();
    command.mcp_servers.clear();
    command.runtime_policy = Some(RuntimeExecutionPolicySnapshot::default());
    let claims = WorkloadIdentityClaims {
        schema_version: 4,
        tenant_id: command.tenant_id,
        application_id: command.application_id,
        workload_identity_id: command.workload_identity_id,
        run_id: command.run_id,
        session_id: command.session_id,
        workspace_id: command.workspace_id,
        agent_version_id: command.agent_version_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_incarnation_id,
        model_policy_id: command.model_policy_id,
        model_policy_digest: command.model_policy_digest.clone(),
        authorized_mcp_servers: Default::default(),
        audiences: BTreeSet::from(["model-gateway".into(), "checkpoint-gateway".into()]),
        scopes: BTreeSet::from([
            "model.execute".into(),
            "checkpoint.read".into(),
            "checkpoint.write".into(),
        ]),
        issued_at_unix_ms: command.issued_at.timestamp_millis(),
        expires_at_unix_ms: command.lease_expires_at.timestamp_millis(),
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let token = format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(signing_key.sign(signing_input.as_bytes()).to_bytes())
    );
    command.workload_token = serde_json::from_value(json!(token)).unwrap();
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());

    WorkerProcessor::verify_execution_workload_identity(&command, &verifier, command.issued_at)
        .expect("the complete signed command must be admitted");

    command.workspace_id = Uuid::now_v7();
    assert!(matches!(
        WorkerProcessor::verify_execution_workload_identity(&command, &verifier, command.issued_at),
        Err(WorkerAssignmentError::WorkloadIdentityBindingMismatch)
    ));
}

#[test]
fn v21_worker_admission_requires_federation_authority_when_mcp_is_configured() {
    let signing_key = SigningKey::from_bytes(&[65; 32]);
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V12_EXAMPLE).unwrap();
    command.schema_version = agent_protocol::RUN_EXECUTION_SCHEMA_VERSION;
    command.application_id = Uuid::now_v7();
    command.workload_identity_id = Uuid::now_v7();
    command.session_branch = Some(SessionBranchSnapshot::new(Uuid::now_v7(), 1, Vec::new()));
    command.skill_snapshots.clear();
    command.runtime_policy = Some(RuntimeExecutionPolicySnapshot::default());
    command.mcp_servers = vec![McpServerSnapshot {
        server_id: Uuid::now_v7(),
        name: "search".into(),
        endpoint: "https://mcp.example.test/rpc".into(),
        credential_envelope_base64: String::new(),
        oauth_credential_id: None,
        required: false,
        tool_effect_overrides: BTreeMap::new(),
        protocol_revision: agent_protocol::McpProtocolRevision::V2025_06_18,
        client_capabilities: BTreeSet::new(),
    }];
    let server = &command.mcp_servers[0];
    let wire_server = agent_model_gateway_protocol::v1::McpServerRef {
        server_id: server.server_id.to_string(),
        name: server.name.clone(),
        endpoint: server.endpoint.clone(),
        credential_envelope_json: Vec::new(),
        oauth_credential_id: String::new(),
        protocol_revision: server.protocol_revision.as_str().to_string(),
        client_capabilities: Vec::new(),
    };
    let claims = WorkloadIdentityClaims {
        schema_version: 4,
        tenant_id: command.tenant_id,
        application_id: command.application_id,
        workload_identity_id: command.workload_identity_id,
        run_id: command.run_id,
        session_id: command.session_id,
        workspace_id: command.workspace_id,
        agent_version_id: command.agent_version_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_incarnation_id,
        model_policy_id: command.model_policy_id,
        model_policy_digest: command.model_policy_digest.clone(),
        authorized_mcp_servers: BTreeMap::from([(
            server.server_id,
            agent_model_gateway_protocol::mcp_server_authorization_digest(&wire_server),
        )]),
        audiences: BTreeSet::from(["model-gateway".into(), "checkpoint-gateway".into()]),
        scopes: BTreeSet::from([
            "model.execute".into(),
            "checkpoint.read".into(),
            "checkpoint.write".into(),
        ]),
        issued_at_unix_ms: command.issued_at.timestamp_millis(),
        expires_at_unix_ms: command.lease_expires_at.timestamp_millis(),
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let token = format!(
        "{signing_input}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(signing_key.sign(signing_input.as_bytes()).to_bytes())
    );
    command.workload_token = serde_json::from_value(json!(token)).unwrap();

    assert!(matches!(
        WorkerProcessor::verify_execution_workload_identity(
            &command,
            &WorkloadTokenVerifier::new(signing_key.verifying_key()),
            command.issued_at,
        ),
        Err(WorkerAssignmentError::InvalidWorkloadIdentity(_))
    ));
}

#[test]
fn checkpoint_recovery_rejects_runtime_policy_drift_before_resuming() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    command.schema_version = 10;
    command.skill_snapshots.clear();
    let mut runtime_policy = RuntimeExecutionPolicySnapshot {
        schema_version: 1,
        ..RuntimeExecutionPolicySnapshot::default()
    };
    runtime_policy.tool_execution.max_concurrent_tools = 1;
    runtime_policy.mcp_discovery.max_attempts_per_server = 1;
    runtime_policy.mcp_discovery.initial_retry_backoff_ms = 0;
    command.runtime_policy = Some(runtime_policy);
    command.validate().unwrap();
    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let replacement_incarnation_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.worker_incarnation_id = replacement_incarnation_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    replacement_command
        .runtime_policy
        .as_mut()
        .unwrap()
        .tool_execution
        .timeout_ms = 45_000;
    replacement_command.validate().unwrap();
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_worker_id,
        replacement_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();

    assert_eq!(
        replacement
            .restore(
                replacement_command.clone(),
                checkpoint,
                replacement_command.issued_at + Duration::seconds(1),
            )
            .expect_err("a recovered Run must retain the policy it accepted"),
        WorkerAssignmentError::CheckpointIdentityMismatch
    );
}

#[test]
fn cumulative_model_usage_reduces_the_next_turn_budget_and_survives_restore() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_tokens = 100;
    command.budget.max_cost_cents = 2;
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Usage {
                input_tokens: 20,
                output_tokens: 30,
                cost_micros: 6_000,
            },
        )
        .unwrap();
    assert_eq!(
        original
            .prepare_model_invocation(command.attempt_id)
            .unwrap()
            .invocation
            .max_output_tokens,
        50
    );
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    assert_eq!(
        replacement
            .prepare_model_invocation(replacement_command.attempt_id)
            .unwrap()
            .invocation
            .max_output_tokens,
        50
    );
}

#[test]
fn over_budget_usage_is_published_before_a_terminal_budget_failure() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_tokens = 100;
    command.budget.max_cost_cents = 1;
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    let usage = worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Usage {
                input_tokens: 80,
                output_tokens: 21,
                cost_micros: 9_000,
            },
        )
        .unwrap();
    assert_eq!(usage.event_type, "model.usage");
    assert_eq!(
        worker
            .pending_budget_exhaustion(command.attempt_id)
            .unwrap(),
        Some(BudgetDimension::Tokens)
    );

    let terminal = worker
        .terminate_pending_budget_exhaustion(command.attempt_id)
        .unwrap()
        .expect("over-budget usage must have one terminal follow-up");
    assert_eq!(terminal.event_type, "run.failed");
    assert_eq!(terminal.payload["kind"], "budget_exhausted");
    assert_eq!(terminal.payload["dimension"], "tokens");
    assert!(worker.attempt_is_terminal(command.attempt_id).unwrap());
}

#[test]
fn checkpointed_budget_exhaustion_recovers_as_terminal_instead_of_reinvoking_the_model() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_tokens = 100;
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Usage {
                input_tokens: 60,
                output_tokens: 41,
                cost_micros: 1,
            },
        )
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    assert_eq!(
        replacement
            .recovery_action(replacement_command.attempt_id)
            .unwrap(),
        WorkerRecoveryAction::TerminateBudgetExceeded(BudgetDimension::Tokens)
    );
}

#[test]
fn exact_budget_can_still_finish_successfully_but_cannot_start_a_tool_turn() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_tokens = 100;
    command.budget.max_cost_cents = 1;
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        2,
        "0.1.0".into(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Usage {
                input_tokens: 75,
                output_tokens: 25,
                cost_micros: 10_000,
            },
        )
        .unwrap();
    let succeeded = worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        )
        .unwrap();
    assert_eq!(succeeded.event_type, "run.succeeded");

    let mut tool_command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    tool_command.run_id = Uuid::now_v7();
    tool_command.attempt_id = Uuid::now_v7();
    tool_command.budget.max_tokens = 100;
    tool_command.budget.max_cost_cents = 1;
    worker
        .accept(tool_command.clone(), tool_command.issued_at)
        .unwrap();
    worker.start(tool_command.attempt_id).unwrap();
    worker
        .apply_model_event(
            tool_command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_after_exact_budget".into(),
                name: "unused".into(),
                arguments: json!({}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            tool_command.attempt_id,
            ModelStreamEvent::Usage {
                input_tokens: 75,
                output_tokens: 25,
                cost_micros: 10_000,
            },
        )
        .unwrap();
    let exhausted = worker
        .apply_model_event(
            tool_command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    assert_eq!(exhausted.event_type, "run.failed");
    assert_eq!(exhausted.payload["kind"], "budget_exhausted");
}

#[test]
fn tool_turn_preserves_assistant_text_call_and_bound_result() {
    let mut command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Allow,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:read".into()]),
            },
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        })
        .unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();

    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::TextDelta {
                text: "I will inspect the file first. ".into(),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::TextDelta {
                text: "The result will support the answer.".into(),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_42".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let planned = worker
        .plan_next_tool_call(command.attempt_id)
        .expect("completed tool turn must be executable");
    let agent_kernel::ToolPlan::Execute(execution) = planned.plan else {
        panic!("allow policy must execute without approval");
    };
    assert_eq!(planned.event.event_type, "tool.execution.requested");
    assert_eq!(
        worker
            .record_tool_execution_started(
                command.attempt_id,
                &execution.call.id,
                "changed-binding",
            )
            .expect_err("execution start must be bound to the planned request"),
        WorkerAssignmentError::ToolExecutionBindingMismatch
    );
    assert_eq!(
        worker
            .record_bound_tool_result(
                command.attempt_id,
                execution.call.id.clone(),
                &execution.binding_digest,
                json!({"text":"untrusted contents"}),
                false,
            )
            .expect_err("a result cannot precede the durable execution-start event"),
        WorkerAssignmentError::ToolExecutionNotStarted
    );
    let started = worker
        .record_tool_execution_started(
            command.attempt_id,
            &execution.call.id,
            &execution.binding_digest,
        )
        .unwrap();
    assert_eq!(started.event_type, "tool.execution.started");
    assert_eq!(
        started.payload["execution"]["binding_digest"],
        execution.binding_digest
    );
    let result = worker
        .record_bound_tool_result(
            command.attempt_id,
            execution.call.id,
            &execution.binding_digest,
            json!({"text":"runtime contents"}),
            false,
        )
        .unwrap();
    assert_eq!(result.payload["binding_digest"], execution.binding_digest);

    let next = worker.prepare_model_invocation(command.attempt_id).unwrap();
    assert_eq!(next.invocation.tools.len(), 1);
    assert_eq!(next.invocation.tools[0].name, "read_file");
    assert_eq!(next.invocation.messages.len(), 3);
    assert_eq!(next.invocation.messages[1].content.len(), 2);
    let assistant_text = next.invocation.messages[1].content[0]
        .body
        .as_ref()
        .expect("assistant narrative must be present");
    let agent_model_gateway_protocol::v1::content_part::Body::Text(text) = assistant_text else {
        panic!("assistant history must preserve text before the tool call");
    };
    assert_eq!(
        text.text,
        "I will inspect the file first. The result will support the answer."
    );
    let assistant_part = next.invocation.messages[1].content[1]
        .body
        .as_ref()
        .expect("assistant tool call must be present");
    let agent_model_gateway_protocol::v1::content_part::Body::ToolCall(call) = assistant_part
    else {
        panic!("assistant history must preserve the tool call");
    };
    assert_eq!(call.tool_call_id, "call_42");
    let tool_part = next.invocation.messages[2].content[0]
        .body
        .as_ref()
        .expect("tool result must be present");
    let agent_model_gateway_protocol::v1::content_part::Body::ToolResult(result) = tool_part else {
        panic!("next turn must contain a tool result");
    };
    assert_eq!(result.tool_call_id, "call_42");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.content_json).unwrap(),
        json!({"text":"runtime contents"})
    );
}

fn parallel_read_command() -> RunExecutionCommand {
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    let mut command = signed_v6_child_command(&signing_key);
    command.schema_version = 17;
    command.runtime_policy = Some(RuntimeExecutionPolicySnapshot::default());
    command
}

fn register_parallel_read(worker: &mut WorkerProcessor) {
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "workspace.read_text".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Allow,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "c".repeat(64),
                required_scopes: BTreeSet::from(["tool:workspace.read".into()]),
            },
            description: "Read one workspace file".into(),
            input_schema: json!({"type":"object","required":["path"]}),
        })
        .unwrap();
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    worker.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "local-skill-key",
        signing_key.verifying_key(),
    ));
}

fn prepare_two_parallel_reads(
    worker: &mut WorkerProcessor,
    command: &RunExecutionCommand,
) -> (
    agent_protocol::ToolExecutionRequest,
    agent_protocol::ToolExecutionRequest,
) {
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    for (id, path) in [("call_first", "FIRST.txt"), ("call_second", "SECOND.txt")] {
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::ToolCall {
                    id: id.into(),
                    name: "workspace.read_text".into(),
                    arguments: json!({"path": path}),
                },
            )
            .unwrap();
    }
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let first = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let second = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(first) = first.plan else {
        panic!("first pure Tool must execute");
    };
    let agent_kernel::ToolPlan::Execute(second) = second.plan else {
        panic!("second pure Tool must execute");
    };
    (first, second)
}

/// The production break this catches is the transcript adopting executor
/// completion order instead of the assistant's Tool Call order.
#[test]
fn parallel_tool_results_commit_in_original_call_order() {
    let command = parallel_read_command();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    register_parallel_read(&mut worker);
    let (first, second) = prepare_two_parallel_reads(&mut worker, &command);
    worker
        .begin_ordered_tool_batch(command.attempt_id, &[first.clone(), second.clone()])
        .unwrap();
    for request in [&first, &second] {
        worker
            .record_tool_execution_started(
                command.attempt_id,
                &request.call.id,
                &request.binding_digest,
            )
            .unwrap();
    }

    let early = worker
        .record_bound_tool_result_ordered(
            command.attempt_id,
            second.call.id.clone(),
            &second.binding_digest,
            json!({"text":"second"}),
            false,
        )
        .unwrap();
    assert!(
        early.is_empty(),
        "a later result must wait for its predecessor"
    );
    let committed = worker
        .record_bound_tool_result_ordered(
            command.attempt_id,
            first.call.id.clone(),
            &first.binding_digest,
            json!({"text":"first"}),
            false,
        )
        .unwrap();
    assert_eq!(
        committed
            .iter()
            .map(|event| event.payload["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["call_first", "call_second"]
    );

    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();
    let tool_ids = invocation
        .invocation
        .messages
        .iter()
        .filter_map(|message| message.content.first())
        .filter_map(|part| part.body.as_ref())
        .filter_map(|body| match body {
            agent_model_gateway_protocol::v1::content_part::Body::ToolResult(result) => {
                Some(result.tool_call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_ids, vec!["call_first", "call_second"]);
}

/// The production break this catches is a transport reversing the batch before
/// the durable commit order is frozen.
#[test]
fn ordered_tool_batch_rejects_a_transport_reordered_request_list() {
    let command = parallel_read_command();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    register_parallel_read(&mut worker);
    let (first, second) = prepare_two_parallel_reads(&mut worker, &command);

    assert_eq!(
        worker
            .begin_ordered_tool_batch(command.attempt_id, &[second, first])
            .unwrap_err(),
        WorkerAssignmentError::InvalidParallelToolBatch
    );
}

/// The production break this catches is a crash discarding an already-finished
/// later result or replaying it while the earlier pure Tool is retried.
#[test]
fn checkpoint_recovers_a_partially_completed_ordered_tool_batch() {
    let command = parallel_read_command();
    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    register_parallel_read(&mut original);
    let (first, second) = prepare_two_parallel_reads(&mut original, &command);
    original
        .begin_ordered_tool_batch(command.attempt_id, &[first.clone(), second.clone()])
        .unwrap();
    for request in [&first, &second] {
        original
            .record_tool_execution_started(
                command.attempt_id,
                &request.call.id,
                &request.binding_digest,
            )
            .unwrap();
    }
    assert!(
        original
            .record_bound_tool_result_ordered(
                command.attempt_id,
                second.call.id.clone(),
                &second.binding_digest,
                json!({"text":"second"}),
                false,
            )
            .unwrap()
            .is_empty()
    );
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    register_parallel_read(&mut replacement);
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    let WorkerRecoveryAction::RetryToolBatch(retry) = replacement
        .recovery_action(replacement_command.attempt_id)
        .unwrap()
    else {
        panic!("the unfinished prefix must recover as a bounded Tool batch");
    };
    assert_eq!(
        retry
            .iter()
            .map(|request| request.call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_first"]
    );
    let retry = retry.into_iter().next().unwrap();
    replacement
        .replan_recovered_tool(replacement_command.attempt_id, &retry.call.id)
        .unwrap();
    replacement
        .record_tool_execution_started(
            replacement_command.attempt_id,
            &retry.call.id,
            &retry.binding_digest,
        )
        .unwrap();
    let committed = replacement
        .record_bound_tool_result_ordered(
            replacement_command.attempt_id,
            retry.call.id,
            &retry.binding_digest,
            json!({"text":"first"}),
            false,
        )
        .unwrap();
    assert_eq!(
        committed
            .iter()
            .map(|event| event.payload["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["call_first", "call_second"]
    );
}

#[test]
fn checkpoint_restores_transcript_on_a_new_fenced_attempt() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::RestrictedContainer,
            implementation_digest: "a".repeat(64),
            required_scopes: BTreeSet::from(["workspace:read".into()]),
        },
        description: "Read a workspace file".into(),
        input_schema: json!({"type":"object","required":["path"]}),
    };
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.register_tool(definition.clone()).unwrap();
    original
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    original.start(command.attempt_id).unwrap();
    let private_state = ProviderPrivateState {
        provider_id: "openai-primary".into(),
        protocol: "openai_responses".into(),
        model: "gpt-agent".into(),
        format: "openai.responses.reasoning.v1".into(),
        data: "{\"encrypted_content\":\"enc-resume\",\"id\":\"rs_resume\"}".into(),
    };
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Reasoning {
                summary: vec!["Prepared the safe read.".into()],
                private_state: Some(private_state.clone()),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_resume".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = original.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(execution) = planned.plan else {
        panic!("pure tool must be executable");
    };
    original
        .record_tool_execution_started(
            command.attempt_id,
            &execution.call.id,
            &execution.binding_digest,
        )
        .unwrap();
    original
        .record_bound_tool_result(
            command.attempt_id,
            execution.call.id,
            &execution.binding_digest,
            json!({"text":"restored contents"}),
            false,
        )
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement.register_tool(definition).unwrap();

    let restored = replacement
        .restore(
            replacement_command.clone(),
            checkpoint.clone(),
            replacement_command.issued_at + Duration::seconds(1),
        )
        .expect("a fenced replacement attempt must restore the full transcript");
    let duplicate = replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(2),
        )
        .expect("JetStream redelivery must reuse the exact restore receipt");
    let invocation = replacement
        .prepare_model_invocation(replacement_command.attempt_id)
        .unwrap();

    assert_eq!(restored.event.event_type, "run.restored");
    assert_eq!(duplicate, restored);
    assert_eq!(restored.event.attempt_id, replacement_command.attempt_id);
    assert_eq!(invocation.invocation.messages.len(), 3);
    let reasoning = invocation.invocation.messages[1].content[0]
        .body
        .as_ref()
        .expect("reasoning item must survive checkpoint restore");
    let agent_model_gateway_protocol::v1::content_part::Body::Reasoning(reasoning) = reasoning
    else {
        panic!("assistant transcript must retain protocol-neutral reasoning");
    };
    assert_eq!(reasoning.summary, ["Prepared the safe read."]);
    let restored_private = reasoning
        .private_state
        .as_ref()
        .expect("same-provider continuation state must survive restore");
    assert_eq!(restored_private.provider_id, private_state.provider_id);
    assert_eq!(restored_private.protocol, private_state.protocol);
    assert_eq!(restored_private.model, private_state.model);
    assert_eq!(restored_private.format, private_state.format);
    assert_eq!(restored_private.data, private_state.data);
    assert_eq!(
        invocation.invocation.attempt_id,
        replacement_command.attempt_id.to_string()
    );
}

#[test]
fn compaction_summarizes_an_old_prefix_keeps_a_complete_tool_tail_and_restores_exactly() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V12_EXAMPLE).unwrap();
    command.schema_version = agent_protocol::RUN_EXECUTION_SCHEMA_VERSION;
    command.application_id = Uuid::now_v7();
    command.workload_identity_id = Uuid::now_v7();
    command.session_branch = Some(SessionBranchSnapshot::new(Uuid::now_v7(), 1, Vec::new()));
    command.delegated_scopes.insert("workspace:read".into());
    let signing_key = SigningKey::from_bytes(&[63; 32]);
    let mut skill: agent_protocol::SkillSnapshot = serde_json::from_value(json!({
        "schema_version": 1,
        "application_id": command.application_id,
        "skill_version_id": Uuid::now_v7(),
        "name": "compaction-fixture",
        "semantic_version": "1.0.0",
        "description": "Activate the bounded read fixture.",
        "instructions": "Use the bounded read fixture when needed.",
        "tool_names": ["read_file"],
        "supported_platforms": ["darwin-arm64", "linux-arm64", "linux-x86_64"],
        "min_runtime_version": "0.1.0",
        "artifact_digest": "0".repeat(64),
        "signing_key_id": "compaction-test-key",
        "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0_u8; 64])
    }))
    .unwrap();
    skill.artifact_digest = skill.expected_artifact_digest(command.tenant_id);
    let signature =
        signing_key.sign(format!("agent-runtime-skill-v1.{}", skill.artifact_digest).as_bytes());
    skill.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());
    command.skill_snapshots = vec![skill];
    let runtime_policy = RuntimeExecutionPolicySnapshot {
        context_compaction: agent_protocol::ContextCompactionPolicySnapshot {
            enabled: true,
            trigger_bytes: 4_096,
            retain_bytes: 1_024,
            max_summary_tokens: 256,
        },
        ..RuntimeExecutionPolicySnapshot::default()
    };
    command.runtime_policy = Some(runtime_policy);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::RestrictedContainer,
            implementation_digest: "a".repeat(64),
            required_scopes: BTreeSet::from(["workspace:read".into()]),
        },
        description: "Read a workspace file".into(),
        input_schema: json!({"type":"object","required":["path"]}),
    };
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "compaction-test-key",
        signing_key.verifying_key(),
    ));
    worker.register_tool(definition.clone()).unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();

    for (id, narrative, path, contents) in [
        (
            "call_old",
            "I will inspect the older file.",
            "old.txt",
            "o".repeat(2_500),
        ),
        (
            "call_recent",
            "I will inspect the recent file.",
            "recent.txt",
            "r".repeat(2_500),
        ),
    ] {
        if id == "call_recent" {
            worker
                .apply_model_event(
                    command.attempt_id,
                    ModelStreamEvent::Reasoning {
                        summary: vec!["Selected the recent evidence.".into()],
                        private_state: Some(ProviderPrivateState {
                            provider_id: "openai-primary".into(),
                            protocol: "openai_responses".into(),
                            model: "gpt-agent".into(),
                            format: "openai.responses.reasoning.v1".into(),
                            data: "{\"encrypted_content\":\"enc-compact\",\"id\":\"rs_compact\"}"
                                .into(),
                        }),
                    },
                )
                .unwrap();
        }
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::TextDelta {
                    text: narrative.into(),
                },
            )
            .unwrap();
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({"path":path}),
                },
            )
            .unwrap();
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::Completed {
                    reason: ModelFinishReason::ToolCalls,
                },
            )
            .unwrap();
        let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
        let agent_kernel::ToolPlan::Execute(execution) = planned.plan else {
            panic!("pure Tool must execute");
        };
        worker
            .record_tool_execution_started(
                command.attempt_id,
                &execution.call.id,
                &execution.binding_digest,
            )
            .unwrap();
        worker
            .record_bound_tool_result(
                command.attempt_id,
                execution.call.id,
                &execution.binding_digest,
                json!({"text":contents}),
                false,
            )
            .unwrap();
    }

    let prepared = worker
        .prepare_transcript_compaction(command.attempt_id)
        .unwrap()
        .expect("the bounded transcript must cross the configured trigger");
    assert!(prepared.invocation.tools.is_empty());
    assert_eq!(prepared.invocation.max_output_tokens, 256);
    assert_eq!(prepared.source_message_count, 3);
    assert_eq!(prepared.retained_message_count, 2);
    assert_eq!(prepared.invocation.messages.len(), 5);

    let compacted = worker
        .apply_transcript_compaction(
            command.attempt_id,
            &prepared.binding_digest,
            "The older turn inspected old.txt and returned bounded contents.",
            40,
            20,
            600,
        )
        .unwrap();
    assert_eq!(compacted.event_type, "context.compacted");
    assert_eq!(compacted.payload["source_message_count"], 3);
    assert_eq!(compacted.payload["retained_message_count"], 2);

    let expected = worker
        .prepare_model_invocation(command.attempt_id)
        .unwrap()
        .invocation
        .messages;
    assert_eq!(expected.len(), 4);
    assert_eq!(
        expected[0].role,
        agent_model_gateway_protocol::v1::ModelRole::System as i32
    );
    let summary = expected[1].content[0].body.as_ref().unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::Text(summary) = summary else {
        panic!("compaction summary must remain ordinary user context");
    };
    assert!(summary.text.contains("The older turn inspected old.txt"));
    let recent_reasoning = expected[2].content[0].body.as_ref().unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::Reasoning(recent_reasoning) =
        recent_reasoning
    else {
        panic!("retained assistant message must keep private reasoning state");
    };
    assert_eq!(recent_reasoning.summary, ["Selected the recent evidence."]);
    assert_eq!(
        recent_reasoning.private_state.as_ref().unwrap().data,
        "{\"encrypted_content\":\"enc-compact\",\"id\":\"rs_compact\"}"
    );
    let recent_call = expected[2].content[2].body.as_ref().unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::ToolCall(recent_call) = recent_call
    else {
        panic!("retained assistant message must keep its Tool call");
    };
    assert_eq!(recent_call.tool_call_id, "call_recent");
    let recent_result = expected[3].content[0].body.as_ref().unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::ToolResult(recent_result) =
        recent_result
    else {
        panic!("retained Tool call must keep its result");
    };
    assert_eq!(recent_result.tool_call_id, "call_recent");

    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.worker_incarnation_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_worker_id,
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement.set_skill_artifact_verifier(SkillArtifactVerifier::new(
        "compaction-test-key",
        signing_key.verifying_key(),
    ));
    replacement.register_tool(definition).unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at,
        )
        .unwrap();
    let restored = replacement
        .prepare_model_invocation(replacement_command.attempt_id)
        .unwrap()
        .invocation
        .messages;
    assert_eq!(restored, expected);
}

#[test]
fn worker_rejects_an_executor_whose_implementation_differs_from_the_model_tool_catalog() {
    let mut worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "workspace.read_text".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::TrustedNative,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["tool:workspace.read".into()]),
            },
            description: "Read trusted workspace text".into(),
            input_schema: serde_json::json!({"type":"object"}),
        })
        .unwrap();

    assert!(matches!(
        worker.validate_tool_executor(
            "workspace.read_text",
            SandboxClass::TrustedNative,
            &"b".repeat(64)
        ),
        Err(WorkerAssignmentError::ToolExecutorConfiguration(_))
    ));
    worker
        .validate_tool_executor(
            "workspace.read_text",
            SandboxClass::TrustedNative,
            &"a".repeat(64),
        )
        .unwrap();
}

#[test]
fn native_workspace_tool_is_explicitly_enabled_and_registered_with_ask_policy() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("trusted-workspace-tool");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let workspace_root = temporary.path().join("workspaces");
    std::fs::create_dir(&workspace_root).unwrap();

    assert!(
        prepare_trusted_workspace_tool(false, PathBuf::new(), PathBuf::new())
            .unwrap()
            .is_none()
    );
    let configured = prepare_trusted_workspace_tool(true, executable, workspace_root.clone())
        .unwrap()
        .unwrap();

    let read = configured
        .tools
        .iter()
        .find(|tool| tool.definition.descriptor.name == "workspace.read_text")
        .expect("the read tool is prepared");
    assert_eq!(read.definition.descriptor.effect, ToolEffect::Pure);
    assert_eq!(read.definition.descriptor.approval, ApprovalMode::Ask);
    assert_eq!(
        read.definition.descriptor.sandbox,
        SandboxClass::TrustedNative
    );
    assert_eq!(
        read.definition.descriptor.implementation_digest,
        read.executor.implementation_digest()
    );
    assert_eq!(
        read.definition.descriptor.required_scopes,
        BTreeSet::from(["tool:workspace.read".into()])
    );
    assert_eq!(configured.workspace_root, workspace_root);
}

#[test]
fn restore_rejects_tool_catalog_drift_but_accepts_ambiguous_execution_for_classification() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "publish".into(),
            effect: ToolEffect::NonIdempotent,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::Kata,
            implementation_digest: "b".repeat(64),
            required_scopes: BTreeSet::from(["workspace:write".into()]),
        },
        description: "Publish once".into(),
        input_schema: json!({"type":"object"}),
    };
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.register_tool(definition.clone()).unwrap();
    original
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_publish".into(),
                name: "publish".into(),
                arguments: json!({"release":"v1"}),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = original.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(execution) = planned.plan else {
        panic!("tool must be executable");
    };
    let started = original
        .record_tool_execution_started(
            command.attempt_id,
            &execution.call.id,
            &execution.binding_digest,
        )
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);

    let mut drifted = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let mut changed = definition.clone();
    changed.description = "Changed implementation contract".into();
    drifted.register_tool(changed).unwrap();
    assert_eq!(
        drifted
            .restore(
                replacement_command.clone(),
                checkpoint.clone(),
                replacement_command.issued_at + Duration::seconds(1),
            )
            .unwrap_err(),
        WorkerAssignmentError::CheckpointToolCatalogMismatch
    );

    let mut matching = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    matching.register_tool(definition).unwrap();
    let restored = matching
        .restore(replacement_command, checkpoint, Utc::now())
        .expect("an ambiguous execution must restore before becoming indeterminate");
    assert_eq!(restored.event.event_type, "run.restored");
    let WorkerRecoveryAction::TerminateIndeterminate(uncertainty) = matching
        .recovery_action(restored.accepted.attempt_id)
        .expect("ambiguous execution must become an explicit terminal action")
    else {
        panic!("ambiguous execution must never be retried");
    };
    assert_eq!(uncertainty.request, execution);
    assert_eq!(uncertainty.source_attempt_id, command.attempt_id);
    assert_eq!(uncertainty.started_event_id, started.event_id);
    assert_eq!(uncertainty.started_sequence, started.sequence);

    let terminal = matching
        .terminate_uncertain_tool(restored.accepted.attempt_id)
        .expect("classification must materialize a stable terminal event");
    assert_eq!(terminal.event_type, "run.indeterminate");
    assert_eq!(terminal.payload["tool_call_id"], "call_publish");
    assert_eq!(terminal.payload["tool_name"], "publish");
    assert_eq!(terminal.payload["binding_digest"], execution.binding_digest);
    assert_eq!(terminal.payload["effect"], "non_idempotent");
    assert_eq!(terminal.payload["sandbox"], "kata");
    assert_eq!(
        terminal.payload["source_attempt_id"],
        command.attempt_id.to_string()
    );
    assert_eq!(
        terminal.payload["started_event_id"],
        started.event_id.to_string()
    );
    assert_eq!(terminal.payload["started_sequence"], started.sequence);
    assert_eq!(terminal.payload["replay_safe"], false);
    assert_eq!(terminal.payload["reason"], "tool_outcome_unknown");
    assert_eq!(
        matching
            .terminate_uncertain_tool(restored.accepted.attempt_id)
            .unwrap(),
        terminal
    );
    assert_eq!(
        matching.status(restored.accepted.attempt_id).unwrap(),
        RunStatus::Indeterminate
    );
}

#[test]
fn restored_replay_safe_tool_is_replanned_under_the_new_attempt_before_execution() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::RestrictedContainer,
            implementation_digest: "a".repeat(64),
            required_scopes: BTreeSet::from(["workspace:read".into()]),
        },
        description: "Read a workspace file".into(),
        input_schema: json!({"type":"object"}),
    };
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    original.register_tool(definition.clone()).unwrap();
    original
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_retry".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = original.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::Execute(request) = planned.plan else {
        panic!("pure tool must execute");
    };
    original
        .record_tool_execution_started(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();
    let checkpoint_sequence = checkpoint.sequence;

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    replacement.register_tool(definition).unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    let WorkerRecoveryAction::RetryTool(retry) = replacement
        .recovery_action(replacement_command.attempt_id)
        .unwrap()
    else {
        panic!("a started pure tool must be retried, not sent back to the model");
    };
    assert_eq!(retry.binding_digest, request.binding_digest);
    let replanned = replacement
        .replan_recovered_tool(replacement_command.attempt_id, &retry.call.id)
        .unwrap();
    let duplicate = replacement
        .replan_recovered_tool(replacement_command.attempt_id, &retry.call.id)
        .unwrap();
    assert_eq!(replanned.event_type, "tool.execution.requested");
    assert_eq!(replanned.attempt_id, replacement_command.attempt_id);
    assert_eq!(replanned.sequence, checkpoint_sequence + 2);
    assert_eq!(duplicate, replanned);
}

#[test]
fn restored_pending_approval_is_rebound_before_a_new_decision_can_execute() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let definition = WorkerToolDefinition {
        descriptor: ToolDescriptor {
            name: "write_file".into(),
            effect: ToolEffect::Idempotent,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::Kata,
            implementation_digest: "b".repeat(64),
            required_scopes: BTreeSet::from(["workspace:write".into()]),
        },
        description: "Write a workspace file".into(),
        input_schema: json!({"type":"object"}),
    };
    let mut original = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    original.register_tool(definition.clone()).unwrap();
    original
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_approval_resume".into(),
                name: "write_file".into(),
                arguments: json!({"path":"result.txt"}),
            },
        )
        .unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = original.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::ApprovalRequired(approval) = planned.plan else {
        panic!("ask policy must pause for approval");
    };
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();
    let checkpoint_sequence = checkpoint.sequence;
    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".into(),
    )
    .unwrap();
    replacement.register_tool(definition).unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();

    assert_eq!(
        replacement
            .recovery_action(replacement_command.attempt_id)
            .unwrap(),
        WorkerRecoveryAction::WaitForApproval
    );
    let rebound = replacement
        .rebind_recovered_approval(replacement_command.attempt_id)
        .unwrap();
    let duplicate = replacement
        .rebind_recovered_approval(replacement_command.attempt_id)
        .unwrap();

    assert_eq!(rebound.event_type, "approval.rebound");
    assert_eq!(rebound.attempt_id, replacement_command.attempt_id);
    assert_eq!(rebound.sequence, checkpoint_sequence + 2);
    assert_eq!(
        rebound.payload["approval"]["approval_id"],
        approval.approval_id.to_string()
    );
    assert_eq!(duplicate, rebound);
    let rebound_checkpoint = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();
    let rebound_state: serde_json::Value =
        serde_json::from_slice(&rebound_checkpoint.state).unwrap();
    assert_eq!(
        rebound_state["execution_time"]["active"], false,
        "recovery work may be charged, but the rebound approval must park the clock again"
    );
}

#[test]
fn policy_denied_tool_becomes_a_model_visible_error_without_execution() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:read".into()]);
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Deny,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:read".into()]),
            },
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object"}),
        })
        .unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_denied".into(),
                name: "read_file".into(),
                arguments: json!({"path":"secret.txt"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();

    assert!(matches!(planned.plan, agent_kernel::ToolPlan::Denied(_)));
    assert_eq!(planned.event.event_type, "tool.denied");
    let result = planned
        .followup_event
        .expect("policy denial must produce a model-visible result");
    assert_eq!(result.event_type, "tool.result");
    assert_eq!(result.payload["is_error"], true);
    assert!(worker.prepare_model_invocation(command.attempt_id).is_ok());
}

#[test]
fn approval_required_tool_cannot_execute_with_a_changed_binding() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "shell".into(),
                effect: ToolEffect::Unknown,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::Kata,
                implementation_digest: "b".repeat(64),
                required_scopes: BTreeSet::from(["workspace:write".into()]),
            },
            description: "Run a command".into(),
            input_schema: json!({"type":"object","required":["command"]}),
        })
        .unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: json!({"command":"cargo test"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::ApprovalRequired(approval) = planned.plan else {
        panic!("shell must wait for approval");
    };
    let parked_checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let parked_state: serde_json::Value = serde_json::from_slice(&parked_checkpoint.state).unwrap();
    assert_eq!(
        parked_state["execution_time"]["active"], false,
        "an approval checkpoint must stop the Worker execution clock"
    );

    assert_eq!(planned.event.event_type, "approval.required");
    assert_eq!(
        worker
            .approve_tool_call(command.attempt_id, approval.approval_id, "changed-binding")
            .expect_err("approval must be bound to the reviewed request"),
        WorkerAssignmentError::ApprovalBindingMismatch
    );
    let (resumed, execution) = worker
        .approve_tool_call(
            command.attempt_id,
            approval.approval_id,
            &approval.execution.binding_digest,
        )
        .unwrap();
    assert_eq!(resumed.event_type, "run.resumed");
    assert_eq!(execution.call.id, "call_shell");
    let resumed_checkpoint = worker.checkpoint(command.attempt_id).unwrap();
    let resumed_state: serde_json::Value =
        serde_json::from_slice(&resumed_checkpoint.state).unwrap();
    assert_eq!(
        resumed_state["execution_time"]["active"], true,
        "applying the approval must start a new monotonic execution slice"
    );
}

#[test]
fn active_worker_attempt_expires_its_duration_budget_as_a_timeout_terminal() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.budget.max_duration_seconds = 1;
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    assert!(worker.expired_duration_attempt_ids().is_empty());

    std::thread::sleep(std::time::Duration::from_millis(1_050));

    assert_eq!(
        worker.expired_duration_attempt_ids(),
        vec![command.attempt_id]
    );
    let terminal = worker.timeout_duration(command.attempt_id).unwrap();
    assert_eq!(terminal.event_type, "run.timed_out");
    assert_eq!(terminal.payload["kind"], "duration_budget_exhausted");
    assert_eq!(
        worker.status(command.attempt_id).unwrap(),
        agent_protocol::RunStatus::TimedOut
    );
}

#[test]
fn signed_allow_once_decision_resumes_the_exact_attempt_and_is_idempotent() {
    let (mut worker, command, approval) = waiting_approval_worker();
    let issued_at = command.issued_at + Duration::seconds(2);
    let decision = ToolApprovalDecisionCommand {
        schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: command.tenant_id,
        run_id: command.run_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_id,
        approval_id: approval.approval_id,
        approval_version: 2,
        binding_digest: approval.execution.binding_digest.clone(),
        decision: ToolApprovalDecision::AllowOnce,
        issued_at,
        expires_at: issued_at + Duration::minutes(5),
    };

    let first = worker
        .apply_tool_approval(decision.clone(), issued_at + Duration::seconds(1))
        .unwrap();
    let duplicate = worker
        .apply_tool_approval(decision, issued_at + Duration::seconds(2))
        .unwrap();

    assert_eq!(duplicate, first);
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].event_type, "run.resumed");
    assert_eq!(first.execution.unwrap().call.id, "call_shell");
}

#[test]
fn denial_resumes_with_a_bound_error_result_and_never_exposes_execution() {
    let (mut worker, command, approval) = waiting_approval_worker();
    let issued_at = command.issued_at + Duration::seconds(2);
    let outcome = worker
        .apply_tool_approval(
            ToolApprovalDecisionCommand {
                schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                message_id: Uuid::now_v7(),
                tenant_id: command.tenant_id,
                run_id: command.run_id,
                attempt_id: command.attempt_id,
                worker_id: command.worker_id,
                worker_incarnation_id: command.worker_id,
                approval_id: approval.approval_id,
                approval_version: 2,
                binding_digest: approval.execution.binding_digest,
                decision: ToolApprovalDecision::Deny,
                issued_at,
                expires_at: issued_at + Duration::minutes(5),
            },
            issued_at + Duration::seconds(1),
        )
        .unwrap();

    assert!(outcome.execution.is_none());
    assert_eq!(outcome.events.len(), 2);
    assert_eq!(outcome.events[0].event_type, "run.resumed");
    assert_eq!(outcome.events[1].event_type, "tool.result");
    assert_eq!(outcome.events[1].payload["tool_call_id"], "call_shell");
    assert_eq!(outcome.events[1].payload["is_error"], true);
    let next = worker.prepare_model_invocation(command.attempt_id).unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::ToolResult(result) =
        next.invocation.messages.last().unwrap().content[0]
            .body
            .as_ref()
            .unwrap()
    else {
        panic!("denial must be represented as a tool result");
    };
    assert_eq!(result.tool_call_id, "call_shell");
}

fn waiting_approval_worker() -> (
    WorkerProcessor,
    RunExecutionCommand,
    agent_protocol::ToolApprovalRequest,
) {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from(["workspace:write".into()]);
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "shell".into(),
                effect: ToolEffect::Unknown,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::Kata,
                implementation_digest: "b".repeat(64),
                required_scopes: BTreeSet::from(["workspace:write".into()]),
            },
            description: "Run a command".into(),
            input_schema: json!({"type":"object","required":["command"]}),
        })
        .unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: json!({"command":"cargo test"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();
    let planned = worker.plan_next_tool_call(command.attempt_id).unwrap();
    let agent_kernel::ToolPlan::ApprovalRequired(approval) = planned.plan else {
        panic!("shell must wait for approval");
    };
    (worker, command, approval)
}

#[test]
fn unauthorized_tool_call_is_not_lost_when_policy_planning_fails() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker
        .register_tool(WorkerToolDefinition {
            descriptor: ToolDescriptor {
                name: "read_file".into(),
                effect: ToolEffect::Pure,
                approval: ApprovalMode::Allow,
                sandbox: SandboxClass::RestrictedContainer,
                implementation_digest: "a".repeat(64),
                required_scopes: BTreeSet::from(["workspace:read".into()]),
            },
            description: "Read a workspace file".into(),
            input_schema: json!({"type":"object"}),
        })
        .unwrap();
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call_denied".into(),
                name: "read_file".into(),
                arguments: json!({"path":"secret.txt"}),
            },
        )
        .unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::ToolCalls,
            },
        )
        .unwrap();

    assert!(matches!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::ToolConfiguration(_))
    ));
    assert!(matches!(
        worker.plan_next_tool_call(command.attempt_id),
        Err(WorkerAssignmentError::ToolConfiguration(_))
    ));
}

#[test]
fn command_for_another_worker_is_rejected() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");

    assert_eq!(
        worker
            .accept(command.clone(), command.issued_at + Duration::seconds(1))
            .expect_err("wrong target must fail"),
        WorkerAssignmentError::WrongWorker
    );
}

#[test]
fn stable_worker_restart_rejects_a_command_for_the_previous_incarnation() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    let previous_incarnation = Uuid::now_v7();
    let current_incarnation = Uuid::now_v7();
    command.schema_version = 2;
    command.worker_incarnation_id = previous_incarnation;
    let mut restarted = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        current_incarnation,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".into(),
    )
    .unwrap();

    assert_eq!(
        restarted.accept(command, Utc::now()),
        Err(WorkerAssignmentError::WrongWorkerIncarnation)
    );
}

#[test]
fn expired_lease_is_rejected_before_execution() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");

    assert_eq!(
        worker
            .accept(
                command.clone(),
                command.lease_expires_at + Duration::milliseconds(1)
            )
            .expect_err("expired assignment must fail"),
        WorkerAssignmentError::LeaseExpired
    );
    assert_eq!(worker.heartbeat(Utc::now()).active_runs, 0);
}

#[test]
fn terminal_event_holds_capacity_until_durable_ack_is_reported() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();

    let terminal = worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::Completed {
                reason: ModelFinishReason::Stop,
            },
        )
        .expect("completion produces a terminal event");

    assert_eq!(terminal.event_type, "run.succeeded");
    assert_eq!(worker.heartbeat(Utc::now()).active_runs, 1);
    worker
        .acknowledge_terminal(command.attempt_id, terminal.event_id)
        .expect("durable event acknowledgement releases capacity");
    assert_eq!(worker.heartbeat(Utc::now()).active_runs, 0);
}

#[test]
fn cancellation_wins_a_race_and_cannot_be_overwritten_by_late_model_completion() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();

    let cancelled = worker
        .cancel(command.attempt_id)
        .expect("active attempt can be cancelled");
    let duplicate = worker
        .cancel(command.attempt_id)
        .expect("redelivered cancellation is idempotent");

    assert_eq!(cancelled.event_type, "run.cancelled");
    assert_eq!(duplicate, cancelled);
    assert_eq!(
        worker
            .apply_model_event(
                command.attempt_id,
                ModelStreamEvent::Completed {
                    reason: ModelFinishReason::Stop,
                },
            )
            .expect_err("late provider completion must not replace cancellation"),
        WorkerAssignmentError::AttemptAlreadyTerminal
    );
}

#[test]
fn exact_steering_cancels_the_old_model_turn_and_appends_input_once() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    let old_model_turn = worker.cancellation_token(command.attempt_id).unwrap();
    let steering = steering_command(&command, "Focus on the authorization failure first.");

    let applied = worker
        .apply_steering(steering.clone(), steering.issued_at + Duration::seconds(1))
        .expect("a fresh exact steering command is applied");
    let duplicate = worker
        .apply_steering(steering, command.issued_at + Duration::seconds(4))
        .expect("exact redelivery returns the durable receipt");

    assert_eq!(applied.event_type, "run.steer.applied");
    assert_eq!(duplicate, applied);
    assert!(old_model_turn.is_cancelled());
    assert!(
        !worker
            .cancellation_token(command.attempt_id)
            .unwrap()
            .is_cancelled()
    );
    let invocation = worker.prepare_model_invocation(command.attempt_id).unwrap();
    assert_eq!(invocation.invocation.messages.len(), 2);
    let body = invocation.invocation.messages.last().unwrap().content[0]
        .body
        .as_ref()
        .unwrap();
    let agent_model_gateway_protocol::v1::content_part::Body::Text(text) = body else {
        panic!("steering must append one user text message");
    };
    assert_eq!(text.text, "Focus on the authorization failure first.");
}

#[test]
fn recovered_steering_receipt_rebinds_without_appending_the_input_twice() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.accept(command.clone(), command.issued_at).unwrap();
    original.start(command.attempt_id).unwrap();
    let steering = steering_command(&command, "Review only the authorization evidence.");
    original
        .apply_steering(steering.clone(), steering.issued_at + Duration::seconds(1))
        .unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command.clone();
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = Utc::now();
    replacement_command.lease_expires_at = replacement_command.issued_at + Duration::minutes(5);
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + Duration::seconds(1),
        )
        .unwrap();
    let rebound = RunSteeringCommand::new(
        Uuid::now_v7(),
        steering.steering_id,
        RunSteeringTarget {
            tenant_id: replacement_command.tenant_id,
            run_id: replacement_command.run_id,
            attempt_id: replacement_command.attempt_id,
            worker_id: replacement_command.worker_id,
            worker_incarnation_id: replacement_command.worker_incarnation_id,
        },
        RunSteeringRequest {
            input: steering.input,
            issued_at: replacement_command.issued_at,
            expires_at: replacement_command.issued_at + Duration::seconds(30),
        },
    );

    let event = replacement
        .apply_steering(rebound.clone(), rebound.issued_at + Duration::seconds(1))
        .expect("a restored receipt emits a replacement-attempt acknowledgement");

    assert_eq!(event.event_type, "run.steer.applied");
    assert_eq!(event.attempt_id, replacement_command.attempt_id);
    assert_eq!(event.sequence, 4);
    let invocation = replacement
        .prepare_model_invocation(replacement_command.attempt_id)
        .unwrap();
    assert_eq!(invocation.invocation.messages.len(), 2);
}

#[test]
fn steering_fails_closed_after_a_tool_call_has_started() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V2_EXAMPLE).unwrap();
    let mut worker = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.accept(command.clone(), command.issued_at).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-side-effect".into(),
                name: "unknown-tool".into(),
                arguments: json!({}),
            },
        )
        .unwrap();
    let steering = steering_command(&command, "Ignore that tool and do something else.");

    assert_eq!(
        worker
            .apply_steering(steering.clone(), steering.issued_at + Duration::seconds(1))
            .unwrap_err(),
        WorkerAssignmentError::SteeringUnsafe
    );
}

#[test]
fn only_fresh_cancellation_for_the_exact_assignment_is_applied() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    let issued_at = command.issued_at + Duration::seconds(2);
    let cancellation = RunCancellationCommand {
        schema_version: RUN_CANCELLATION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: command.tenant_id,
        run_id: command.run_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_id,
        issued_at,
        expires_at: issued_at + Duration::seconds(30),
        reason: "user_requested".into(),
    };

    let event = worker
        .apply_cancellation(cancellation.clone(), issued_at + Duration::seconds(1))
        .expect("matching fresh cancellation is accepted");
    assert_eq!(event.event_type, "run.cancelled");

    let mut wrong_run = cancellation;
    wrong_run.run_id = Uuid::now_v7();
    assert_eq!(
        worker
            .apply_cancellation(wrong_run, issued_at + Duration::seconds(1))
            .expect_err("run identity cannot be changed"),
        WorkerAssignmentError::AttemptConflict
    );
}

#[test]
fn targeted_cancellation_trips_the_active_attempt_model_signal() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    let mut worker = WorkerProcessor::new(
        command.worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .expect("valid worker config");
    worker
        .accept(command.clone(), command.issued_at + Duration::seconds(1))
        .unwrap();
    worker.start(command.attempt_id).unwrap();
    let signal = worker
        .cancellation_token(command.attempt_id)
        .expect("active attempt exposes one cancellation signal");
    let issued_at = command.issued_at + Duration::seconds(2);

    worker
        .apply_cancellation(
            RunCancellationCommand {
                schema_version: RUN_CANCELLATION_SCHEMA_VERSION,
                message_id: Uuid::now_v7(),
                tenant_id: command.tenant_id,
                run_id: command.run_id,
                attempt_id: command.attempt_id,
                worker_id: command.worker_id,
                worker_incarnation_id: command.worker_id,
                issued_at,
                expires_at: issued_at + Duration::seconds(30),
                reason: "user_requested".into(),
            },
            issued_at + Duration::seconds(1),
        )
        .unwrap();

    assert!(signal.is_cancelled());
}

#[test]
fn the_worker_prepares_a_write_tool_contained_separately_from_the_read_tool() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("agent-trusted-workspace-tool");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let workspace_root = temporary.path().join("workspaces");
    std::fs::create_dir(&workspace_root).unwrap();

    let configured = prepare_trusted_workspace_tool(true, executable, workspace_root.clone())
        .unwrap()
        .unwrap();

    let names = configured
        .tools
        .iter()
        .map(|tool| tool.definition.descriptor.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["workspace.read_text", "workspace.write_text", "shell.exec"]
    );

    let write = configured
        .tools
        .iter()
        .find(|tool| tool.definition.descriptor.name == "workspace.write_text")
        .expect("the write tool must be prepared");
    // A write changes the user's files, so it is approval gated and must never
    // be auto-retried after an ambiguous failure.
    assert_eq!(
        write.definition.descriptor.effect,
        ToolEffect::NonIdempotent
    );
    assert_eq!(write.definition.descriptor.approval, ApprovalMode::Ask);
    assert_eq!(
        write.definition.descriptor.required_scopes,
        BTreeSet::from(["tool:workspace.write".to_string()])
    );

    // Separate executors so the read tool keeps running under a profile that
    // grants no writes at all.
    let read = configured
        .tools
        .iter()
        .find(|tool| tool.definition.descriptor.name == "workspace.read_text")
        .expect("the read tool must still be prepared");
    assert_eq!(read.definition.descriptor.effect, ToolEffect::Pure);
    assert_eq!(
        read.definition.descriptor.required_scopes,
        BTreeSet::from(["tool:workspace.read".to_string()])
    );
    assert!(
        !Arc::ptr_eq(&read.executor, &write.executor),
        "the read and write tools must not share one executor"
    );

    // Shell is the widest capability the Worker installs, so its policy is
    // pinned here rather than left to be noticed if it is ever relaxed.
    let shell = configured
        .tools
        .iter()
        .find(|tool| tool.definition.descriptor.name == "shell.exec")
        .expect("the shell tool must be prepared");
    assert_eq!(
        shell.definition.descriptor.effect,
        ToolEffect::NonIdempotent
    );
    assert_eq!(shell.definition.descriptor.approval, ApprovalMode::Ask);
    // Its own scope: granting shell must not ride along with granting writes.
    assert_eq!(
        shell.definition.descriptor.required_scopes,
        BTreeSet::from(["tool:shell.exec".to_string()])
    );
    assert!(
        !Arc::ptr_eq(&shell.executor, &write.executor)
            && !Arc::ptr_eq(&shell.executor, &read.executor),
        "the shell tool must not share an executor with the file tools"
    );
}
