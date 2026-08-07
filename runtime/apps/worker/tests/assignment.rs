use agent_protocol::{
    ApprovalMode, AutoApproval, BudgetDimension, ModelFinishReason, ModelStreamEvent,
    RUN_CANCELLATION_SCHEMA_VERSION, RunCancellationCommand, RunExecutionCommand,
    RunSteeringCommand, RunSteeringRequest, RunSteeringTarget, SandboxClass,
    SubagentResultDelivery, SubagentResultOutcome, SubagentResultSource,
    TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision, ToolApprovalDecisionCommand,
    ToolDescriptor, ToolEffect, WorkloadIdentityRenewalCommand,
};
use agent_runtime_worker::{
    SkillArtifactVerifier, WorkerAssignmentError, WorkerProcessor, WorkerRecoveryAction,
    WorkerToolDefinition, WorkloadIdentityRenewalOutcome, materialize_native_workspace,
    prepare_trusted_workspace_tool,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
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
                auto_approval: AutoApproval::Never,
            },
            description: "Read bounded workspace text".into(),
            input_schema: json!({"type":"object"}),
        })
        .unwrap();
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
                    auto_approval: AutoApproval::Never,
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
        vec!["agent.spawn"]
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
        run_id: command.run_id,
        attempt_id: command.attempt_id,
        worker_id: command.worker_id,
        worker_incarnation_id: command.worker_incarnation_id,
        model_policy_id: command.model_policy_id,
        model_policy_digest: String::new(),
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
                auto_approval: AutoApproval::Never,
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
fn allowed_tool_result_is_added_to_the_next_model_turn_without_losing_call_identity() {
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
                auto_approval: AutoApproval::Never,
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
    let assistant_part = next.invocation.messages[1].content[0]
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
            auto_approval: AutoApproval::Never,
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
    assert_eq!(
        invocation.invocation.attempt_id,
        replacement_command.attempt_id.to_string()
    );
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
                auto_approval: AutoApproval::Never,
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
fn restore_rejects_tool_catalog_drift_and_ambiguous_non_idempotent_execution() {
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
            auto_approval: AutoApproval::Never,
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
    original
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
    assert_eq!(
        matching
            .restore(replacement_command, checkpoint, Utc::now(),)
            .unwrap_err(),
        WorkerAssignmentError::AmbiguousToolExecution
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
            auto_approval: AutoApproval::Never,
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
            auto_approval: AutoApproval::Never,
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
                auto_approval: AutoApproval::Never,
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
                auto_approval: AutoApproval::Never,
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
                auto_approval: AutoApproval::Never,
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
                auto_approval: AutoApproval::Never,
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
