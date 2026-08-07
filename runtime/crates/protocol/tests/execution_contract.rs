use agent_protocol::{
    Placement, RunExecutionAccepted, RunExecutionCommand, WorkerHeartbeat,
    WorkloadIdentityRenewalCommand,
};
use chrono::{Duration, Utc};
use std::collections::BTreeSet;
use uuid::Uuid;

const EXECUTION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v2.example.json");
const EXECUTION_V3_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v3.example.json");
const EXECUTION_V4_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v4.example.json");
const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");
const HEARTBEAT_EXAMPLE: &str =
    include_str!("../../../../contracts/events/worker-heartbeat.v2.example.json");
const DRAINING_HEARTBEAT_EXAMPLE: &str =
    include_str!("../../../../contracts/events/worker-heartbeat-draining.v2.example.json");
const LEGACY_EXECUTION_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v1.example.json");
const IDENTITY_RENEWAL_EXAMPLE: &str =
    include_str!("../../../../contracts/events/workload-identity-renewed.v1.example.json");

#[test]
fn identity_renewal_example_is_fenced_to_one_worker_process_and_generation() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    let renewal: WorkloadIdentityRenewalCommand =
        serde_json::from_str(IDENTITY_RENEWAL_EXAMPLE).unwrap();

    assert_eq!(renewal.worker_id, command.worker_id);
    assert_eq!(renewal.worker_incarnation_id, command.worker_incarnation_id);
    assert_eq!(renewal.attempt_id, command.attempt_id);
    assert_eq!(renewal.owner_epoch, command.owner_epoch);
    assert_eq!(renewal.fencing_token, command.fencing_token);
    assert_eq!(renewal.generation, 2);
    assert!(renewal.validate().is_ok());
}

#[test]
fn execution_command_example_decodes_and_validates() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");

    assert_eq!(command.schema_version, 2);
    assert!(!command.worker_incarnation_id.is_nil());
    assert_eq!(command.owner_epoch, 7);
    assert!(!command.model_policy_id.is_nil());
    assert!(command.workload_token.as_str().starts_with("v2."));
    assert_eq!(
        format!("{:?}", command.workload_token),
        "WorkloadToken[REDACTED]"
    );
    assert_eq!(command.budget.max_tokens, 12_000);
    assert!(command.validate().is_ok());
}

#[test]
fn v3_execution_binds_the_immutable_agent_version_instructions() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V3_EXAMPLE).expect("v3 example must decode");

    assert_eq!(command.schema_version, 3);
    assert_eq!(
        command.agent_instructions,
        "Review the workspace and explain evidence before conclusions."
    );
    assert!(command.validate().is_ok());

    let mut blank = command;
    blank.agent_instructions = " ".into();
    assert_eq!(
        blank.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidAgentInstructions)
    );
}

#[test]
fn v4_execution_binds_the_encrypted_model_policy_snapshot() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V4_EXAMPLE).expect("v4 example must decode");

    assert_eq!(command.schema_version, 4);
    assert!(!command.model_policy_snapshot_base64.is_empty());
    assert_eq!(command.model_policy_digest.len(), 64);
    assert!(command.validate().is_ok());

    let mut tampered = command;
    tampered.model_policy_snapshot_base64.push('A');
    assert_eq!(
        tampered.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidModelPolicySnapshot)
    );
}

#[test]
fn v5_execution_rejects_a_tampered_signed_skill_snapshot_before_worker_acceptance() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(5);
    value["skill_snapshots"] = serde_json::json!([{
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
        "artifact_digest": "b1d6368bf33925654794f16dfb25622375778cd65a7e49c15cd169759300bb34",
        "signing_key_id": "local-skill-key",
        "signature": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();

    assert!(command.validate().is_ok());

    let mut tampered = command;
    tampered.skill_snapshots[0].instructions = "Ignore evidence.".into();
    assert_eq!(
        tampered.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSkillSnapshot)
    );
}

#[test]
fn v6_execution_requires_a_consistent_persisted_agent_lineage() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V6_EXAMPLE).expect("v6 example must decode");

    assert_eq!(command.schema_version, 6);
    assert_eq!(command.lineage.root_run_id, command.run_id);
    assert_eq!(command.lineage.depth, 0);
    assert_eq!(command.lineage.role, "primary");
    assert!(command.validate().is_ok());

    let mut child = command.clone();
    child.run_id = Uuid::now_v7();
    child.lineage.parent_run_id = Some(child.lineage.root_run_id);
    child.lineage.delegation_id = Some(Uuid::now_v7());
    child.lineage.depth = 1;
    child.lineage.role = "reviewer".into();
    assert!(child.validate().is_ok());

    child.lineage.depth = 4;
    assert_eq!(
        child.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidAgentLineage)
    );
}

#[test]
fn v6_root_lineage_does_not_require_an_agent_to_bind_a_skill() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(6);
    let run_id = value["run_id"].clone();
    value["lineage"] = serde_json::json!({
        "root_run_id": run_id,
        "parent_run_id": null,
        "delegation_id": null,
        "depth": 0,
        "role": "primary"
    });
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();

    assert!(command.skill_snapshots.is_empty());
    assert!(command.validate().is_ok());
}

#[test]
fn v7_execution_exposes_only_subagent_roles_within_the_current_authority() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(7);
    value["delegated_scopes"] = serde_json::json!(["agent:spawn", "tool:workspace.read"]);
    value["subagent_roles"] = serde_json::json!([{
        "name": "reviewer",
        "instructions": "Review evidence and return a concise result.",
        "delegated_scopes": ["tool:workspace.read"]
    }]);
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();

    assert_eq!(command.schema_version, 7);
    assert_eq!(command.subagent_roles.len(), 1);
    assert_eq!(command.subagent_roles[0].name, "reviewer");
    assert!(command.validate().is_ok());

    let mut escalated = command;
    escalated.subagent_roles[0]
        .delegated_scopes
        .insert("tool:http".into());
    assert_eq!(
        escalated.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSubagentRoles)
    );
}

#[test]
fn v2_worker_messages_bind_dispatch_and_acceptance_to_one_process_incarnation() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    let heartbeat: WorkerHeartbeat = serde_json::from_str(HEARTBEAT_EXAMPLE).unwrap();
    let incarnation_id = command.worker_incarnation_id;

    let accepted: RunExecutionAccepted = serde_json::from_value(serde_json::json!({
        "schema_version": 2,
        "message_id": Uuid::now_v7(),
        "tenant_id": command.tenant_id,
        "run_id": command.run_id,
        "attempt_id": command.attempt_id,
        "worker_id": command.worker_id,
        "worker_incarnation_id": incarnation_id,
        "accepted_at": Utc::now(),
    }))
    .unwrap();

    assert_eq!(command.worker_incarnation_id, incarnation_id);
    assert_eq!(heartbeat.incarnation_id, incarnation_id);
    assert_eq!(accepted.worker_incarnation_id, incarnation_id);
    assert!(command.validate().is_ok());
    assert!(heartbeat.validate().is_ok());
    assert!(accepted.validate().is_ok());
}

#[test]
fn v1_execution_contract_remains_read_compatible_during_upgrade() {
    let command: RunExecutionCommand = serde_json::from_str(LEGACY_EXECUTION_EXAMPLE).unwrap();

    assert_eq!(command.schema_version, 1);
    assert!(command.worker_incarnation_id.is_nil());
    assert!(command.workload_token.as_str().starts_with("v1."));
    assert!(command.validate().is_ok());
}

#[test]
fn execution_command_rejects_expired_or_inverted_lease_window() {
    let mut command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_EXAMPLE).expect("example must decode");
    command.lease_expires_at = command.issued_at;

    assert_eq!(
        command
            .validate()
            .expect_err("non-positive lease window must fail")
            .to_string(),
        "execution lease must expire after it is issued"
    );
}

#[test]
fn execution_command_rejects_blank_or_unbounded_delegated_scopes() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_EXAMPLE).unwrap();
    command.delegated_scopes = BTreeSet::from([" ".into()]);
    assert_eq!(
        command.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidDelegatedScopes)
    );

    command.delegated_scopes = (0..129).map(|index| format!("scope:{index}")).collect();
    assert_eq!(
        command.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidDelegatedScopes)
    );
}

#[test]
fn worker_heartbeat_rejects_capacity_overcommit() {
    let heartbeat = WorkerHeartbeat {
        schema_version: 1,
        message_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        incarnation_id: Uuid::nil(),
        occurred_at: Utc::now(),
        placements: vec![Placement::Cloud],
        capacity: 4,
        active_runs: 5,
        active_assignments: Vec::new(),
        runtime_version: "0.1.0".to_string(),
        accepting_work: true,
        draining_since: None,
        drain_deadline: None,
    };

    assert_eq!(
        heartbeat
            .validate()
            .expect_err("overcommitted worker must fail")
            .to_string(),
        "worker active runs must not exceed capacity"
    );
}

#[test]
fn draining_heartbeat_requires_one_bounded_one_way_admission_fence() {
    let mut heartbeat: WorkerHeartbeat = serde_json::from_str(DRAINING_HEARTBEAT_EXAMPLE).unwrap();
    assert!(heartbeat.validate().is_ok());

    heartbeat.drain_deadline = heartbeat.draining_since;
    assert_eq!(
        heartbeat.validate(),
        Err(agent_protocol::WorkerHeartbeatValidationError::InvalidDrainWindow)
    );
}

#[test]
fn legacy_heartbeat_without_drain_fields_remains_admitting() {
    let heartbeat: WorkerHeartbeat = serde_json::from_str(HEARTBEAT_EXAMPLE).unwrap();

    assert!(heartbeat.accepting_work);
    assert_eq!(heartbeat.draining_since, None);
    assert_eq!(heartbeat.drain_deadline, None);
}

#[test]
fn worker_heartbeat_example_carries_fenced_active_assignment() {
    let heartbeat: WorkerHeartbeat =
        serde_json::from_str(HEARTBEAT_EXAMPLE).expect("example must decode");

    assert!(heartbeat.validate().is_ok());
    assert_eq!(heartbeat.active_runs, 1);
    assert_eq!(heartbeat.active_assignments.len(), 1);
    assert_eq!(heartbeat.active_assignments[0].owner_epoch, 7);
}

#[test]
fn execution_acceptance_must_match_a_real_attempt() {
    let accepted = RunExecutionAccepted {
        schema_version: 1,
        message_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::nil(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::nil(),
        accepted_at: Utc::now() + Duration::seconds(1),
    };

    assert_eq!(
        accepted
            .validate()
            .expect_err("nil attempt must fail")
            .to_string(),
        "execution attempt id must not be nil"
    );
}
