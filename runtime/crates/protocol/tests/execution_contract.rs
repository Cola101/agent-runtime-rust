use agent_protocol::{
    ContentPart, HistoryImportSource, Message, Placement, Role, RunExecutionAccepted,
    RunExecutionCommand, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
    SubagentBudgetUsage, SubagentConversationTurn, SubagentResultDelivery, SubagentResultOutcome,
    SubagentResultSource, WorkerHeartbeat, WorkloadIdentityRenewalCommand,
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
const EXECUTION_V10_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v10.example.json");
const EXECUTION_V11_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v11.example.json");
const EXECUTION_V12_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v12.example.json");
const EXECUTION_V13_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v13.example.json");
const EXECUTION_V14_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v14.example.json");
const EXECUTION_V15_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v15.example.json");
const EXECUTION_V16_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v16.example.json");
const EXECUTION_V17_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v17.example.json");
const EXECUTION_V18_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v18.example.json");
const EXECUTION_V19_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v19.example.json");
const EXECUTION_V20_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v20.example.json");
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
    assert_eq!(command.validate(), Ok(()));
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
fn v16_root_execution_binds_an_authoritative_session_branch_without_promoting_history() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V16_EXAMPLE).expect("v16 example must decode");

    assert_eq!(command.validate(), Ok(()));

    let mut tampered = command.clone();
    tampered.session_branch.as_mut().unwrap().history[0].transcript[1].content[0] =
        ContentPart::Text {
            text: "forged".into(),
        };
    assert_eq!(
        tampered.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSessionBranch)
    );

    let mut ambiguous = command;
    ambiguous.history_import = Some(agent_protocol::HistoryImport {
        schema_version: 1,
        source: HistoryImportSource::External,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentPart::Text {
                text: "external".into(),
            }],
        }],
    });
    assert_eq!(
        ambiguous.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSessionBranch)
    );

    let mut downgraded: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V16_EXAMPLE).expect("v16 example must decode");
    downgraded.schema_version = 15;
    assert_eq!(
        downgraded.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSessionBranch)
    );

    let mut child: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V16_EXAMPLE).expect("v16 example must decode");
    child.lineage.root_run_id = Uuid::now_v7();
    child.lineage.parent_run_id = Some(child.lineage.root_run_id);
    child.lineage.delegation_id = Some(Uuid::now_v7());
    child.lineage.depth = 1;
    child.lineage.role = "reviewer".into();
    assert_eq!(
        child.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSessionBranch)
    );
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
fn v20_execution_binds_one_complete_application_scoped_invocation_identity() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V20_EXAMPLE).expect("v20 application identity must decode");

    assert_eq!(command.validate(), Ok(()));

    let mut cross_application_skill = command.clone();
    cross_application_skill.skill_snapshots[0].application_id = Uuid::now_v7();
    assert_eq!(
        cross_application_skill.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSkillSnapshot),
        "a signed Skill from another application must not enter this invocation"
    );

    for missing in [
        "tenant_id",
        "application_id",
        "workload_identity_id",
        "run_id",
        "session_id",
        "workspace_id",
        "agent_version_id",
        "attempt_id",
        "worker_id",
        "worker_incarnation_id",
        "fencing_token",
    ] {
        let mut invalid = command.clone();
        match missing {
            "tenant_id" => invalid.tenant_id = Uuid::nil(),
            "application_id" => invalid.application_id = Uuid::nil(),
            "workload_identity_id" => invalid.workload_identity_id = Uuid::nil(),
            "run_id" => invalid.run_id = Uuid::nil(),
            "session_id" => invalid.session_id = Uuid::nil(),
            "workspace_id" => invalid.workspace_id = Uuid::nil(),
            "agent_version_id" => invalid.agent_version_id = Uuid::nil(),
            "attempt_id" => invalid.attempt_id = Uuid::nil(),
            "worker_id" => invalid.worker_id = Uuid::nil(),
            "worker_incarnation_id" => invalid.worker_incarnation_id = Uuid::nil(),
            "fencing_token" => invalid.fencing_token = Uuid::nil(),
            _ => unreachable!(),
        }
        assert_eq!(
            invalid.validate(),
            Err(agent_protocol::RunExecutionValidationError::InvalidInvocationIdentity),
            "{missing} must be immutable and non-nil in v20"
        );
    }
}

/// OAuth credential material stays inside the Model Gateway credential domain.
/// The Run contract carries only one non-nil stable handle, and an older schema
/// cannot silently accept that new authority-bearing field.
#[test]
fn v21_mcp_oauth_handle_is_stable_authority_and_downgrade_safe() {
    let credential_id = Uuid::now_v7();
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V20_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(21);
    value["mcp_servers"][0]["oauth_credential_id"] = serde_json::json!(credential_id.to_string());

    let command: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(command.validate(), Ok(()));
    assert_eq!(
        command.mcp_servers[0].oauth_credential_id,
        Some(credential_id)
    );
    assert!(command.mcp_servers[0].credential_envelope_base64.is_empty());

    value["schema_version"] = serde_json::json!(20);
    let downgraded: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        downgraded.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpServers),
        "a v20 Worker must reject an OAuth authority it cannot resolve"
    );

    value["schema_version"] = serde_json::json!(21);
    value["mcp_servers"][0]["oauth_credential_id"] = serde_json::json!(Uuid::nil());
    let nil_handle: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        nil_handle.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpServers)
    );

    value["mcp_servers"][0]["oauth_credential_id"] = serde_json::json!(credential_id.to_string());
    value["mcp_servers"][0]["credential_envelope_base64"] =
        serde_json::json!("eyJzZWFsZWQiOnRydWV9");
    let ambiguous: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert_eq!(
        ambiguous.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpServers),
        "static and OAuth credentials must never be active together"
    );
}

#[test]
fn runtime_invocation_context_rejects_an_ambiguous_resource_boundary() {
    let context = RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    };

    assert_eq!(context.validate(), Ok(()));

    let mut invalid = context;
    invalid.application_id = Uuid::nil();
    assert!(invalid.validate().is_err());
}

#[test]
fn v19_execution_without_application_identity_remains_read_compatible() {
    let command: RunExecutionCommand =
        serde_json::from_str(EXECUTION_V19_EXAMPLE).expect("v19 example must decode");

    assert!(command.application_id.is_nil());
    assert_eq!(command.validate(), Ok(()));
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

/// The approval policy is a tenant decision, so it has to arrive in the command
/// rather than live as a constant in the Worker.
///
/// ADR-0039 shipped it as a constant. That meant every tenant granted shell got
/// the same exemption, no tenant administrator could turn it off, and the
/// control plane had no say in a decision that is theirs. v8 carries the policy
/// per Tool so the Worker applies what it was told rather than what it believes.
#[test]
fn v8_execution_carries_the_tool_approval_policy_the_tenant_configured() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(8);
    value["tool_approval_policies"] = serde_json::json!({
        "shell.exec": "provably_read_only_shell_command"
    });

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(command.validate().is_ok());
    assert_eq!(
        command.tool_approval_policies.get("shell.exec"),
        Some(&agent_protocol::AutoApproval::ProvablyReadOnlyShellCommand)
    );
}

/// A command that says nothing about a Tool must mean "ask", not "whatever the
/// Worker prefers". Older commands say nothing about any Tool, so the absent
/// case has to be the safe one.
#[test]
fn an_execution_without_policies_means_every_tool_asks() {
    let value: serde_json::Value = serde_json::from_str(EXECUTION_V4_EXAMPLE).unwrap();
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();

    assert!(command.validate().is_ok());
    assert!(
        command.tool_approval_policies.is_empty(),
        "an older command must not be read as granting an exemption"
    );
}

/// An unknown policy name must not silently degrade to a permissive default.
#[test]
fn an_unrecognised_policy_name_is_refused_rather_than_defaulted() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(8);
    value["tool_approval_policies"] = serde_json::json!({ "shell.exec": "trust_me" });

    assert!(
        serde_json::from_value::<RunExecutionCommand>(value).is_err(),
        "an unknown policy must fail to decode rather than become a default"
    );
}

/// A command that claims an older schema must not carry a v8 field. Accepting
/// one would let a downgraded command smuggle an exemption past a Worker that
/// believes it is speaking the older, policy-free contract.
#[test]
fn a_pre_v8_execution_carrying_policies_is_refused() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(7);
    value["tool_approval_policies"] = serde_json::json!({
        "shell.exec": "provably_read_only_shell_command"
    });

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert_eq!(
        command.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidToolApprovalPolicies)
    );
}

/// A federated MCP server arrives sealed, exactly as a model Provider does.
///
/// The Worker gets the endpoint and the namespace so it can name the tools; it
/// does not get the credential, because the credential is unsealed at the egress
/// hop and never on the machine running the model's suggestions (ADR-0040).
#[test]
fn v9_execution_carries_sealed_mcp_servers() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(9);
    value["delegated_scopes"] = serde_json::json!(["tool:mcp:search"]);
    value["mcp_servers"] = serde_json::json!([{
        "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
        "name": "search",
        "endpoint": "https://mcp.example.com/rpc",
        "credential_envelope_base64": "eyJzZWFsZWQiOnRydWV9"
    }]);

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(command.validate().is_ok());
    assert_eq!(command.mcp_servers.len(), 1);
    assert_eq!(command.mcp_servers[0].name, "search");
}

/// The same downgrade guard v8 has. A command claiming an older schema while
/// carrying v9 servers would federate tools past a Worker that believes it is
/// speaking a contract with no federation in it.
#[test]
fn a_pre_v9_execution_carrying_mcp_servers_is_refused() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(8);
    value["mcp_servers"] = serde_json::json!([{
        "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
        "name": "search",
        "endpoint": "https://mcp.example.com/rpc",
        "credential_envelope_base64": ""
    }]);

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(
        command.validate().is_err(),
        "a downgraded command must not smuggle federated servers"
    );
}

/// A server the AgentVersion does not delegate is not reachable, and shipping it
/// anyway is a pre-authorisation waiting for a scope change to activate.
#[test]
fn an_mcp_server_without_its_delegated_scope_is_refused() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(9);
    value["delegated_scopes"] = serde_json::json!(["tool:workspace.read"]);
    value["mcp_servers"] = serde_json::json!([{
        "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
        "name": "search",
        "endpoint": "https://mcp.example.com/rpc",
        "credential_envelope_base64": ""
    }]);

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(
        command.validate().is_err(),
        "a server nobody delegated must not be carried"
    );
}

/// The name is the namespace in `mcp:<server>/<tool>`. A name carrying a
/// separator could make one server's tool resolve as another's, and the Worker
/// has to refuse that on its own rather than trusting the control plane to have
/// checked -- the whole point of validating a contract on receipt.
#[test]
fn an_mcp_server_name_that_could_forge_a_qualified_tool_name_is_refused() {
    // "1/b" and "9:x" are here because a first pass at this check let any
    // name starting with a digit through regardless of what followed it, and a
    // hostile list without one would have shipped that.
    for hostile in ["a/b", "a:b", "", "UPPER", "1/b", "9:x", "-lead", "a b"] {
        let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
        value["schema_version"] = serde_json::json!(9);
        value["delegated_scopes"] = serde_json::json!([format!("tool:mcp:{hostile}")]);
        value["mcp_servers"] = serde_json::json!([{
            "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
            "name": hostile,
            "endpoint": "https://mcp.example.com/rpc",
            "credential_envelope_base64": ""
        }]);

        let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
        assert!(
            command.validate().is_err(),
            "server name {hostile:?} must be refused"
        );
    }
}

/// Runtime scheduling is part of the Run's meaning, not a Worker preference.
///
/// Before v10, MCP discovery concurrency/deadlines, model failover and Tool
/// execution timeout lived in three different processes as constants. Moving a
/// Run to another host could therefore change all three without changing the
/// command that was accepted. v10 freezes the effective values before any
/// network discovery or model work starts.
#[test]
fn v10_execution_requires_one_bounded_runtime_policy_snapshot() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V10_EXAMPLE).unwrap();

    let command: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert!(command.validate().is_ok());

    value.as_object_mut().unwrap().remove("runtime_policy");
    let missing: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        missing.validate().unwrap_err().to_string(),
        "v10 execution runtime policy is missing, malformed, or carried by an older schema"
    );

    value["runtime_policy"] = serde_json::json!({
        "schema_version": 1,
        "mcp_discovery": {
            "max_concurrent_servers": 17,
            "per_server_timeout_ms": 3_000,
            "total_timeout_ms": 10_000
        },
        "model_failover": {
            "max_provider_attempts": 9,
            "fallback_on": ["authentication"]
        },
        "tool_execution": {
            "timeout_ms": 0
        }
    });
    let unbounded: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert_eq!(
        unbounded.validate().unwrap_err().to_string(),
        "v10 execution runtime policy is missing, malformed, or carried by an older schema"
    );
}

/// A downgraded command must not smuggle a policy past a runtime that believes
/// policy is still host-local. This is the same anti-downgrade boundary as the
/// v8 approval policy and v9 MCP server catalog.
#[test]
fn a_pre_v10_execution_carrying_runtime_policy_is_refused() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(9);
    value["runtime_policy"] = serde_json::json!({
        "schema_version": 1,
        "mcp_discovery": {
            "max_concurrent_servers": 4,
            "per_server_timeout_ms": 3_000,
            "total_timeout_ms": 10_000
        },
        "model_failover": {
            "max_provider_attempts": 1,
            "fallback_on": []
        },
        "tool_execution": {
            "timeout_ms": 30_000
        }
    });

    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert_eq!(
        command.validate().unwrap_err().to_string(),
        "v10 execution runtime policy is missing, malformed, or carried by an older schema"
    );
}

/// The production break this catches is an older Worker silently treating a
/// required server as optional, or silently dropping the retry budget because
/// both fields were added without a schema fence.
#[test]
fn v11_execution_binds_required_mcp_and_bounded_discovery_retries() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V11_EXAMPLE).unwrap();

    let command: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert!(command.validate().is_ok());

    value["schema_version"] = serde_json::json!(10);
    let downgraded: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert_eq!(
        downgraded.validate().unwrap_err().to_string(),
        "v11 MCP availability and discovery retry policy cannot be carried by an older execution schema"
    );
}

/// Existing v10 commands remain optional and single-attempt. This catches a
/// compatibility regression where serde defaults accidentally grant retries
/// or turn an old server into a required dependency.
#[test]
fn v10_mcp_policy_defaults_to_optional_single_attempt_discovery() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V10_EXAMPLE).unwrap();
    assert!(command.validate().is_ok());
    assert_eq!(
        command
            .runtime_policy
            .unwrap()
            .mcp_discovery
            .max_attempts_per_server,
        1
    );
}

/// The production break this catches is context compaction silently inheriting
/// host-local thresholds, or an older policy schema accepting thresholds it
/// does not understand. Either would make the same Run build different model
/// context after placement or recovery.
#[test]
fn runtime_policy_v3_binds_bounded_protocol_neutral_compaction() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V13_EXAMPLE).unwrap();

    let command: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(command.schema_version, 13);
    assert!(command.validate().is_ok());

    value["schema_version"] = serde_json::json!(12);
    let downgraded: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert!(downgraded.validate().is_err());

    value["schema_version"] = serde_json::json!(13);
    value["runtime_policy"]["schema_version"] = serde_json::json!(2);
    let old_policy: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert!(old_policy.validate().is_err());

    value["runtime_policy"]["schema_version"] = serde_json::json!(3);
    value["runtime_policy"]["context_compaction"]["retain_bytes"] = serde_json::json!(4_096);
    let non_shrinking: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(non_shrinking.validate().is_err());
}

/// The production break this catches is a host-local or unbounded Tool fan-out
/// changing the same Run's behavior after placement or Checkpoint recovery.
#[test]
fn runtime_policy_v4_binds_a_bounded_parallel_tool_limit() {
    let policy = RuntimeExecutionPolicySnapshot::default();
    assert_eq!(policy.schema_version, 4);
    assert_eq!(policy.tool_execution.max_concurrent_tools, 4);
    assert!(policy.is_bounded_and_safe());

    let mut unbounded = policy.clone();
    unbounded.tool_execution.max_concurrent_tools = 17;
    assert!(!unbounded.is_bounded_and_safe());

    let mut legacy = policy;
    legacy.schema_version = 3;
    assert!(
        !legacy.is_bounded_and_safe(),
        "an older policy schema must not silently accept parallel Tool semantics"
    );
    legacy.tool_execution.max_concurrent_tools = 1;
    assert!(legacy.is_bounded_and_safe());
}

/// The production break this catches is a v16 Worker accepting a parallel
/// scheduling policy whose Checkpoint state it does not know how to preserve.
#[test]
fn execution_v17_fences_parallel_tool_policy_from_older_workers() {
    let current: RunExecutionCommand = serde_json::from_str(EXECUTION_V17_EXAMPLE).unwrap();
    assert!(current.validate().is_ok());

    let mut value = serde_json::to_value(current).unwrap();
    value["schema_version"] = serde_json::json!(16);

    let downgraded: RunExecutionCommand = serde_json::from_value(value.clone()).unwrap();
    assert!(downgraded.validate().is_err());

    value["schema_version"] = serde_json::json!(17);
    value["runtime_policy"]["schema_version"] = serde_json::json!(3);
    value["runtime_policy"]["tool_execution"]["max_concurrent_tools"] = serde_json::json!(1);
    let old_policy: RunExecutionCommand = serde_json::from_value(value).unwrap();
    assert!(old_policy.validate().is_err());
}

/// An MCP Tool's replay semantics come from the operator-owned Run snapshot,
/// never from an annotation supplied by the remote MCP server. The snapshot
/// must survive decode/encode intact or a command can appear accepted while the
/// Worker silently falls back to `unknown`.
#[test]
fn v18_execution_preserves_operator_owned_mcp_tool_effect_overrides() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V18_EXAMPLE).unwrap();
    assert!(command.validate().is_ok());
    let encoded = serde_json::to_value(command).unwrap();

    assert_eq!(
        encoded["mcp_servers"][0]["tool_effect_overrides"]["web_search"], "idempotent",
        "the Run-frozen operator policy was silently discarded"
    );
}

#[test]
fn mcp_tool_effect_overrides_are_versioned_and_limited_to_declared_tools() {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V17_EXAMPLE).unwrap();
    let signed_skill_source: serde_json::Value =
        serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["skill_snapshots"] = signed_skill_source["skill_snapshots"].clone();
    value["schema_version"] = serde_json::json!(18);
    value["delegated_scopes"] = serde_json::json!(["tool:mcp:search"]);
    value["mcp_servers"] = serde_json::json!([{
        "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
        "name": "search",
        "endpoint": "https://mcp.example.com/rpc",
        "credential_envelope_base64": "",
        "required": true,
        "tool_effect_overrides": {
            "web_search": "idempotent"
        }
    }]);
    let mut command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    command.skill_snapshots[0]
        .tool_names
        .push("mcp:search/web_search".into());
    command.skill_snapshots[0].tool_names.sort();
    command.skill_snapshots[0].artifact_digest =
        command.skill_snapshots[0].expected_artifact_digest(command.tenant_id);
    assert!(
        command.validate().is_ok(),
        "valid v18 override was refused: {:?}",
        command.validate()
    );

    let mut downgraded = command.clone();
    downgraded.schema_version = 17;
    assert!(
        downgraded.validate().is_err(),
        "a pre-v18 command smuggled an effect override"
    );

    let mut undeclared = command;
    undeclared.mcp_servers[0].tool_effect_overrides = std::collections::BTreeMap::from([(
        "delete_everything".into(),
        agent_protocol::ToolEffect::Pure,
    )]);
    assert!(
        undeclared.validate().is_err(),
        "an override outside the signed Skill declaration was accepted"
    );
}

/// MCP 2026-07-28 removes the stateful initialize session and replaces reverse
/// client requests with MRTR.  A Run must freeze that choice: inferring it from
/// whichever server answers after recovery would change both the wire protocol
/// and the authority available to the server.
#[test]
fn v19_freezes_the_mcp_protocol_revision_and_client_capabilities() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V19_EXAMPLE).unwrap();
    assert_eq!(command.validate(), Ok(()));
    assert_eq!(
        serde_json::to_value(&command).unwrap()["mcp_servers"][0]["protocol_revision"],
        "2026-07-28"
    );

    let mut downgraded = command.clone();
    downgraded.schema_version = 18;
    assert_eq!(
        downgraded.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpProtocolPolicy)
    );

    let mut undelegated = command.clone();
    undelegated
        .delegated_scopes
        .remove("mcp:elicitation:search");
    assert_eq!(
        undelegated.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpProtocolPolicy)
    );

    let mut legacy_with_mrtr = command;
    legacy_with_mrtr.mcp_servers[0].protocol_revision =
        agent_protocol::McpProtocolRevision::V2025_06_18;
    assert_eq!(
        legacy_with_mrtr.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpProtocolPolicy)
    );
}

#[test]
fn v22_can_freeze_the_explicit_2025_03_26_legacy_revision() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V20_EXAMPLE).unwrap();
    command.schema_version = agent_protocol::RUN_EXECUTION_SCHEMA_VERSION;
    command.mcp_servers[0].protocol_revision = agent_protocol::McpProtocolRevision::V2025_03_26;
    command.mcp_servers[0].client_capabilities.clear();

    assert_eq!(command.validate(), Ok(()));
    assert_eq!(
        serde_json::to_value(&command).unwrap()["mcp_servers"][0]["protocol_revision"],
        "2025-03-26"
    );

    command.schema_version = 21;
    assert_eq!(
        command.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidMcpProtocolPolicy)
    );
}

#[test]
fn legacy_mcp_servers_decode_to_the_explicit_default_deny_policy() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V18_EXAMPLE).unwrap();
    let server = &command.mcp_servers[0];

    assert_eq!(
        server.protocol_revision,
        agent_protocol::McpProtocolRevision::V2025_06_18
    );
    assert!(server.client_capabilities.is_empty());
}

#[test]
fn v12_child_history_is_role_preserving_bounded_and_downgrade_safe() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V12_EXAMPLE).unwrap();
    assert!(command.validate().is_ok());
    let root_run_id = command.run_id;
    command.run_id = Uuid::now_v7();
    command.lineage.root_run_id = root_run_id;
    command.lineage.parent_run_id = Some(root_run_id);
    command.lineage.delegation_id = Some(command.run_id);
    command.lineage.depth = 1;
    command.lineage.role = "worker".into();
    let prior_child_run_id = Uuid::now_v7();
    let result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: "agent.send:handle:1".into(),
            delegation_id: prior_child_run_id,
            binding_digest: "a".repeat(64),
            child_run_id: prior_child_run_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "first answer"}),
            is_error: false,
        },
    );
    command.subagent_history = vec![SubagentConversationTurn {
        activation_ordinal: 0,
        message_sequence: 0,
        child_run_id: prior_child_run_id,
        input: "first question".into(),
        result,
    }];
    assert!(command.validate().is_ok());

    command.schema_version = 11;
    assert_eq!(
        command.validate().unwrap_err().to_string(),
        "v12 subagent conversation history is malformed or carried by an older schema"
    );
}

#[test]
fn v14_requires_digest_bound_typed_subagent_history_and_rejects_downgrade() {
    let mut command: RunExecutionCommand = serde_json::from_str(EXECUTION_V14_EXAMPLE).unwrap();
    assert_eq!(command.schema_version, 14);
    assert!(command.validate().is_ok());
    let root_run_id = command.run_id;
    command.run_id = Uuid::now_v7();
    command.lineage.root_run_id = root_run_id;
    command.lineage.parent_run_id = Some(root_run_id);
    command.lineage.delegation_id = Some(command.run_id);
    command.lineage.depth = 1;
    command.lineage.role = "worker".into();
    let prior_child_run_id = Uuid::now_v7();
    let rich_result = SubagentResultDelivery::new_with_usage_and_transcript(
        SubagentResultSource {
            tool_call_id: "agent.send:handle:1".into(),
            delegation_id: prior_child_run_id,
            binding_digest: "a".repeat(64),
            child_run_id: prior_child_run_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "first answer"}),
            is_error: false,
        },
        SubagentBudgetUsage {
            tokens: 12,
            cost_micros: 34,
        },
        vec![
            Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "first question".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::Text {
                    text: "first answer".into(),
                }],
            },
        ],
    );
    command.subagent_history = vec![SubagentConversationTurn {
        activation_ordinal: 0,
        message_sequence: 0,
        child_run_id: prior_child_run_id,
        input: "first question".into(),
        result: rich_result,
    }];

    command.schema_version = 14;
    assert!(command.validate().is_ok());

    let mut downgraded = command.clone();
    downgraded.schema_version = 13;
    assert_eq!(
        downgraded.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSubagentHistory)
    );

    let mut incomplete = command;
    incomplete.subagent_history[0].result = SubagentResultDelivery::new(
        SubagentResultSource {
            tool_call_id: "agent.send:handle:1".into(),
            delegation_id: prior_child_run_id,
            binding_digest: "a".repeat(64),
            child_run_id: prior_child_run_id,
            child_terminal_event_id: Uuid::now_v7(),
        },
        SubagentResultOutcome {
            terminal_status: RunStatus::Succeeded,
            content: serde_json::json!({"text": "first answer"}),
            is_error: false,
        },
    );
    assert_eq!(
        incomplete.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidSubagentHistory)
    );
}

#[test]
fn v15_separates_explicit_lower_authority_history_import_from_subagent_history() {
    let command: RunExecutionCommand = serde_json::from_str(EXECUTION_V15_EXAMPLE).unwrap();
    assert_eq!(command.schema_version, 15);
    assert_eq!(
        command.history_import.as_ref().map(|import| import.source),
        Some(HistoryImportSource::Truncated)
    );
    assert!(command.validate().is_ok());

    let mut downgraded = command.clone();
    downgraded.schema_version = 14;
    assert_eq!(
        downgraded.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidHistoryImport)
    );

    let mut ambiguous_order = command;
    let root_run_id = ambiguous_order.run_id;
    ambiguous_order.run_id = Uuid::now_v7();
    ambiguous_order.lineage.root_run_id = root_run_id;
    ambiguous_order.lineage.parent_run_id = Some(root_run_id);
    ambiguous_order.lineage.delegation_id = Some(ambiguous_order.run_id);
    ambiguous_order.lineage.depth = 1;
    ambiguous_order.lineage.role = "worker".into();
    let child_run_id = Uuid::now_v7();
    ambiguous_order.subagent_history = vec![SubagentConversationTurn {
        activation_ordinal: 0,
        message_sequence: 0,
        child_run_id,
        input: "child input".into(),
        result: SubagentResultDelivery::new_with_usage_and_transcript(
            SubagentResultSource {
                tool_call_id: "call_child".into(),
                delegation_id: child_run_id,
                binding_digest: "a".repeat(64),
                child_run_id,
                child_terminal_event_id: Uuid::now_v7(),
            },
            SubagentResultOutcome {
                terminal_status: RunStatus::Succeeded,
                content: serde_json::json!({"text": "child answer"}),
                is_error: false,
            },
            SubagentBudgetUsage::default(),
            vec![
                Message {
                    role: Role::User,
                    content: vec![ContentPart::Text {
                        text: "child input".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Text {
                        text: "child answer".into(),
                    }],
                },
            ],
        ),
    }];
    assert_eq!(
        ambiguous_order.validate(),
        Err(agent_protocol::RunExecutionValidationError::InvalidHistoryImport)
    );
}

/// A Skill has to be able to declare a federated tool by its qualified name.
///
/// Before this the name rules forbade `:` and `/`, so declaring
/// `mcp:search/web_search` made the Skill snapshot invalid and the Run was
/// refused -- for asking for exactly what ADR-0040 exists to provide.
#[test]
fn a_skill_may_declare_a_qualified_federated_tool_name() {
    for (name, expected_valid) in [
        ("mcp:search/web_search", true),
        ("workspace.read_text", true),
        // Two separators would let one server's tool name a different one.
        ("mcp:search/a/b", false),
        ("mcp:se:arch/tool", false),
        ("mcp:/tool", false),
        ("mcp:search/", false),
        ("mcp:search", false),
        ("mcp:UPPER/tool", false),
        ("mcp:search/UPPER", false),
        ("mcp:-lead/tool", false),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
        value["skill_snapshots"][0]["tool_names"] = serde_json::json!([name]);
        let mut command: RunExecutionCommand = serde_json::from_value(value).unwrap();
        // tool_names is inside the artifact digest, so it has to be recomputed
        // or every case fails on the digest and the name rule is never reached.
        let digest = command.skill_snapshots[0].expected_artifact_digest(command.tenant_id);
        command.skill_snapshots[0].artifact_digest = digest;

        assert_eq!(
            command.validate().is_ok(),
            expected_valid,
            "tool name {name:?} should be valid: {expected_valid}, got {:?}",
            command.validate()
        );
    }
}
