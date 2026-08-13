//! Federated MCP tools joining the kernel's Tool registry (ADR-0040).

use agent_kernel::ToolPlan;
use agent_protocol::{
    ApprovalMode, AutoApproval, MCP_INPUT_RESOLUTION_SCHEMA_VERSION, McpElicitationRequest,
    McpInputAction, McpInputResolutionCommand, McpInputResponse, ModelFinishReason,
    ModelStreamEvent, SandboxClass, ToolCall, ToolEffect,
};
use agent_runtime_worker::{WorkerAssignmentError, WorkerProcessor, federated_tool_definitions};
use base64::Engine;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

fn catalog_digest() -> String {
    "c".repeat(64)
}

fn one_tool() -> Vec<(String, String, serde_json::Value)> {
    vec![(
        "mcp:search/web_search".to_owned(),
        "Search the web".to_owned(),
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    )]
}

#[test]
fn a_federated_tool_is_registered_as_federated_and_always_asks() {
    let definitions =
        federated_tool_definitions("search", &catalog_digest(), one_tool(), &BTreeMap::new())
            .unwrap();

    assert_eq!(definitions.len(), 1);
    let descriptor = &definitions[0].descriptor;
    assert_eq!(descriptor.sandbox, SandboxClass::Federated);
    assert_eq!(descriptor.approval, ApprovalMode::Ask);
    assert_eq!(descriptor.effect, ToolEffect::Unknown);
    assert_eq!(
        descriptor.required_scopes,
        BTreeSet::from(["tool:mcp:search".to_owned()])
    );
    assert_eq!(
        descriptor.implementation_digest,
        catalog_digest(),
        "the frozen catalog digest is what a Checkpoint restore recomputes"
    );
}

#[test]
fn a_run_frozen_operator_override_changes_effect_but_never_lowers_approval() {
    let definitions = federated_tool_definitions(
        "search",
        &catalog_digest(),
        one_tool(),
        &BTreeMap::from([("web_search".to_owned(), ToolEffect::Idempotent)]),
    )
    .unwrap();

    let descriptor = &definitions[0].descriptor;
    assert_eq!(descriptor.effect, ToolEffect::Idempotent);
    assert_eq!(descriptor.approval, ApprovalMode::Ask);
    assert_eq!(descriptor.sandbox, SandboxClass::Federated);
}

/// End of the chain the previous slices built: a discovered tool, registered in
/// the kernel, planned with the tenant's own exemption configured -- and it
/// still asks.
#[test]
fn a_registered_federated_tool_still_asks_with_an_exemption_configured() {
    let mut assignment = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_owned(),
    )
    .unwrap();
    for definition in
        federated_tool_definitions("search", &catalog_digest(), one_tool(), &BTreeMap::new())
            .unwrap()
    {
        assignment.register_tool(definition).unwrap();
    }

    let plan = assignment
        .tool_registry()
        .plan(
            ToolCall {
                id: "call-1".into(),
                name: "mcp:search/web_search".into(),
                // Shaped to trigger the one exemption that exists.
                arguments: serde_json::json!({ "command": "ls -la" }),
            },
            &BTreeSet::from(["tool:mcp:search".to_owned()]),
            &BTreeMap::from([(
                "mcp:search/web_search".to_owned(),
                AutoApproval::ProvablyReadOnlyShellCommand,
            )]),
        )
        .unwrap();

    assert!(
        matches!(plan, ToolPlan::ApprovalRequired(_)),
        "a federated tool must ask even with an exemption configured, got {plan:?}"
    );
}

/// A tool named outside its server's namespace would register under a namespace
/// nobody delegated.
#[test]
fn a_tool_outside_its_servers_namespace_is_refused() {
    let hostile = vec![(
        "mcp:other/web_search".to_owned(),
        "d".to_owned(),
        serde_json::json!({"type": "object"}),
    )];

    assert!(
        federated_tool_definitions("search", &catalog_digest(), hostile, &BTreeMap::new()).is_err()
    );
}

/// Without a real catalog digest a Checkpoint restore has nothing to recompute,
/// so the registration is refused rather than given a placeholder.
#[test]
fn a_catalog_digest_that_is_not_a_digest_is_refused() {
    for bad in ["", "not-a-digest", &"z".repeat(64)] {
        assert!(
            federated_tool_definitions("search", bad, one_tool(), &BTreeMap::new()).is_err(),
            "digest {bad:?} must be refused"
        );
    }
}

const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");
const EXECUTION_V18_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v18.example.json");

#[test]
fn mrtr_input_is_checkpointed_before_user_wait_and_only_safe_to_resume_before_dispatch() {
    use ed25519_dalek::Signer;

    let worker_id = Uuid::now_v7();
    let mut worker = processor(worker_id);
    let mut command: agent_protocol::RunExecutionCommand =
        serde_json::from_str(EXECUTION_V18_EXAMPLE).unwrap();
    command.schema_version = 19;
    command.worker_id = worker_id;
    command.worker_incarnation_id = worker_id;
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::minutes(5);
    command
        .delegated_scopes
        .insert("mcp:elicitation:search".into());
    command.mcp_servers[0].protocol_revision = agent_protocol::McpProtocolRevision::V2026_07_28;
    command.mcp_servers[0].client_capabilities =
        BTreeSet::from([agent_protocol::McpClientCapability::Elicitation]);
    command.mcp_servers[0].tool_effect_overrides.clear();
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[91; 32]);
    for skill in &mut command.skill_snapshots {
        skill.tool_names = vec!["mcp:search/web_search".into()];
        let digest = skill.expected_artifact_digest(command.tenant_id);
        skill.artifact_digest = digest.clone();
        skill.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(format!("agent-runtime-skill-v1.{digest}").as_bytes())
                .to_bytes(),
        );
    }
    worker.accept(command.clone(), chrono::Utc::now()).unwrap();
    attach_stub_catalog(&mut worker, command.attempt_id, &catalog_digest()).unwrap();
    worker.start(command.attempt_id).unwrap();
    worker
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-mrtr".into(),
                name: "mcp:search/web_search".into(),
                arguments: serde_json::json!({"query": "invoice"}),
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
    let ToolPlan::ApprovalRequired(approval) =
        worker.plan_next_tool_call(command.attempt_id).unwrap().plan
    else {
        panic!("federated tool must ask")
    };
    let (_, request) = worker
        .approve_tool_call(
            command.attempt_id,
            approval.approval_id,
            &approval.execution.binding_digest,
        )
        .unwrap();
    worker
        .record_tool_execution_started(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
        )
        .unwrap();
    let required = worker
        .record_mcp_input_required(
            command.attempt_id,
            &request.call.id,
            &request.binding_digest,
            command.mcp_servers[0].server_id,
            "search",
            1,
            "opaque-state".into(),
            BTreeMap::from([(
                "confirmation".into(),
                McpElicitationRequest::Form {
                    message: "Confirm".into(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"confirmed": {"type": "boolean"}},
                        "required": ["confirmed"]
                    }),
                    meta: None,
                },
            )]),
        )
        .unwrap();
    assert_eq!(required.event.event_type, "mcp.input.required");
    let pending = required.pending;
    let checkpoint = worker.checkpoint(command.attempt_id).unwrap();

    let replacement_id = Uuid::now_v7();
    let replacement_command = fenced_replacement(command, replacement_id);
    let mut replacement = processor(replacement_id);
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at,
        )
        .unwrap();
    attach_stub_catalog(
        &mut replacement,
        replacement_command.attempt_id,
        &catalog_digest(),
    )
    .unwrap();
    assert!(matches!(
        replacement
            .recovery_action(replacement_command.attempt_id)
            .unwrap(),
        agent_runtime_worker::WorkerRecoveryAction::WaitForMcpInput(ref found)
            if found == &pending
    ));

    let issued_at = chrono::Utc::now();
    let resolution = McpInputResolutionCommand {
        schema_version: MCP_INPUT_RESOLUTION_SCHEMA_VERSION,
        message_id: Uuid::now_v7(),
        tenant_id: replacement_command.tenant_id,
        run_id: replacement_command.run_id,
        attempt_id: replacement_command.attempt_id,
        worker_id: replacement_command.worker_id,
        worker_incarnation_id: replacement_command.worker_incarnation_id,
        input_id: pending.input_id,
        input_version: 1,
        binding_digest: pending.binding_digest.clone(),
        responses: BTreeMap::from([(
            "confirmation".into(),
            McpInputResponse {
                action: McpInputAction::Accept,
                content: Some(serde_json::json!({"confirmed": true})),
                meta: None,
            },
        )]),
        issued_at,
        expires_at: issued_at + chrono::Duration::minutes(5),
    };
    let resolved = replacement
        .apply_mcp_input_resolution(resolution, issued_at)
        .unwrap();
    assert_eq!(resolved.event.event_type, "mcp.input.resolved");
    assert_eq!(resolved.continuation.round, 2);
    let before_dispatch = replacement
        .checkpoint(replacement_command.attempt_id)
        .unwrap();

    let second_id = Uuid::now_v7();
    let second_command = fenced_replacement(replacement_command, second_id);
    let mut second = processor(second_id);
    second
        .restore(
            second_command.clone(),
            before_dispatch,
            second_command.issued_at,
        )
        .unwrap();
    attach_stub_catalog(&mut second, second_command.attempt_id, &catalog_digest()).unwrap();
    assert!(matches!(
        second.recovery_action(second_command.attempt_id).unwrap(),
        agent_runtime_worker::WorkerRecoveryAction::ResumeMcpTool { .. }
    ));
    second
        .record_mcp_continuation_started(second_command.attempt_id, pending.input_id)
        .unwrap();
    let after_dispatch = second.checkpoint(second_command.attempt_id).unwrap();

    let third_id = Uuid::now_v7();
    let third_command = fenced_replacement(second_command, third_id);
    let mut third = processor(third_id);
    third
        .restore(
            third_command.clone(),
            after_dispatch,
            third_command.issued_at,
        )
        .unwrap();
    attach_stub_catalog(&mut third, third_command.attempt_id, &catalog_digest()).unwrap();
    assert!(matches!(
        third.recovery_action(third_command.attempt_id).unwrap(),
        agent_runtime_worker::WorkerRecoveryAction::TerminateIndeterminate(_)
    ));
}

/// A Skill declaring a federated tool must be accepted, and the tool must reach
/// the model.
///
/// Before this, `effective_skill_state` judged every declared name against the
/// Worker's installed tools, so a Skill naming `mcp:search/web_search` failed the
/// whole Run: a federated tool is never installed on a Worker, it is discovered
/// per Run. The Run was refused for declaring something entirely legitimate.
#[test]
fn a_skill_may_declare_a_federated_tool_and_the_model_is_offered_it() {
    let (mut worker, command) = accepted_with_federated_skill("search", true, true);
    let attempt_id = command.attempt_id;

    assert!(
        worker
            .prepare_model_invocation(attempt_id)
            .unwrap()
            .invocation
            .tools
            .iter()
            .all(|tool| tool.name != "mcp:search/web_search"),
        "before discovery the model must not be offered a tool nothing can execute"
    );

    // Discovery attaches the registry, the definitions and the executors.
    let definitions =
        federated_tool_definitions("search", &catalog_digest(), one_tool(), &BTreeMap::new())
            .unwrap();
    let mut registry = worker.tool_registry().clone();
    let mut executors: Vec<(String, std::sync::Arc<dyn agent_tool_runtime::ToolExecutor>)> =
        Vec::new();
    for definition in &definitions {
        registry.register(definition.descriptor.clone()).unwrap();
        executors.push((
            definition.descriptor.name.clone(),
            std::sync::Arc::new(StubFederatedExecutor {
                digest: catalog_digest(),
            }),
        ));
    }
    worker
        .attach_federated_tools(
            attempt_id,
            registry,
            definitions,
            executors,
            agent_runtime_worker::McpDiscoveryPolicy::default(),
        )
        .unwrap();

    let offered = worker
        .prepare_model_invocation(attempt_id)
        .unwrap()
        .invocation
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    assert!(
        offered.contains(&"mcp:search/web_search".to_owned()),
        "the model must be offered the discovered federated tool, got {offered:?}"
    );
}

#[tokio::test]
async fn a_restored_run_rejects_catalog_drift_then_executes_after_an_exact_reattach() {
    let (mut original, command) = accepted_with_federated_skill("search", true, true);
    attach_stub_catalog(&mut original, command.attempt_id, &"c".repeat(64)).unwrap();
    original.start(command.attempt_id).unwrap();
    original
        .apply_model_event(
            command.attempt_id,
            ModelStreamEvent::ToolCall {
                id: "call-search".into(),
                name: "mcp:search/web_search".into(),
                arguments: serde_json::json!({"query":"checkpoint recovery"}),
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
    let ToolPlan::ApprovalRequired(approval) = original
        .plan_next_tool_call(command.attempt_id)
        .unwrap()
        .plan
    else {
        panic!("a federated tool must wait for approval");
    };
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    // Schema 5 predates the federated binding set. Such a checkpoint cannot
    // prove which MCP catalog it approved, so recovery must fail closed.
    let mut legacy_state: serde_json::Value = serde_json::from_slice(&checkpoint.state).unwrap();
    legacy_state["schema_version"] = serde_json::json!(5);
    legacy_state
        .as_object_mut()
        .unwrap()
        .remove("federated_tool_bindings");
    let legacy_checkpoint = agent_protocol::CheckpointSnapshot::new(
        checkpoint.run_id,
        checkpoint.tenant_id,
        checkpoint.session_id,
        checkpoint.attempt_id,
        checkpoint.status,
        checkpoint.sequence,
        serde_json::to_vec(&legacy_state).unwrap(),
    );
    let legacy_worker_id = Uuid::now_v7();
    let legacy_command = fenced_replacement(command.clone(), legacy_worker_id);
    let mut legacy_replacement = processor(legacy_worker_id);
    assert_eq!(
        legacy_replacement
            .restore(
                legacy_command.clone(),
                legacy_checkpoint,
                legacy_command.issued_at + chrono::Duration::seconds(1),
            )
            .expect_err("a pre-authority-binding MCP checkpoint must fail before restoration"),
        WorkerAssignmentError::CheckpointIdentityMismatch
    );

    let replacement_worker_id = Uuid::now_v7();
    let replacement_command = fenced_replacement(command, replacement_worker_id);
    let mut replacement = processor(replacement_worker_id);
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + chrono::Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(
        replacement
            .verify_restored_federated_tools(replacement_command.attempt_id)
            .expect_err("recovery must not continue before MCP tools are reattached"),
        WorkerAssignmentError::CheckpointToolCatalogMismatch
    );

    assert_eq!(
        attach_stub_catalog(
            &mut replacement,
            replacement_command.attempt_id,
            &"d".repeat(64),
        )
        .expect_err("a changed MCP catalog must not resume a frozen approval"),
        WorkerAssignmentError::CheckpointToolCatalogMismatch
    );

    attach_stub_catalog(
        &mut replacement,
        replacement_command.attempt_id,
        &"c".repeat(64),
    )
    .expect("the exact frozen MCP catalog must be reattached");
    replacement
        .verify_restored_federated_tools(replacement_command.attempt_id)
        .expect("the exact restored MCP bindings satisfy the recovery gate");
    assert_eq!(
        replacement
            .rebind_recovered_approval(replacement_command.attempt_id)
            .unwrap()
            .event_type,
        "approval.rebound"
    );
    let (_, request) = replacement
        .approve_tool_call(
            replacement_command.attempt_id,
            approval.approval_id,
            &approval.execution.binding_digest,
        )
        .unwrap();
    let executor = replacement
        .federated_executor(replacement_command.attempt_id, &request.call.name)
        .expect("the recovered execution must use the reattached executor");
    let result = executor
        .execute(
            request,
            agent_tool_runtime::ToolExecutionContext {
                tenant_id: replacement_command.tenant_id,
                application_id: replacement_command.application_id,
                workload_identity_id: replacement_command.workload_identity_id,
                run_id: replacement_command.run_id,
                session_id: replacement_command.session_id,
                workspace_id: replacement_command.workspace_id,
                agent_version_id: replacement_command.agent_version_id,
                attempt_id: replacement_command.attempt_id,
                workspace_root: std::path::PathBuf::from("."),
                timeout: std::time::Duration::from_secs(1),
                cancellation: tokio_util::sync::CancellationToken::new(),
                requested_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
    assert!(!result.is_error);
}

fn fenced_replacement(
    mut command: agent_protocol::RunExecutionCommand,
    worker_id: Uuid,
) -> agent_protocol::RunExecutionCommand {
    command.message_id = Uuid::now_v7();
    command.attempt_id = Uuid::now_v7();
    command.worker_id = worker_id;
    command.worker_incarnation_id = worker_id;
    command.owner_epoch += 1;
    command.fencing_token = Uuid::now_v7();
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::minutes(5);
    command
}

/// A Skill still cannot widen anything. Declaring a server the command does not
/// carry, or one whose scope was never delegated, is refused exactly as an
/// unavailable native tool is.
#[test]
fn a_skill_cannot_declare_a_federated_tool_it_was_not_granted() {
    for (server_registered, scope_delegated) in [(false, true), (true, false), (false, false)] {
        let worker_id = Uuid::now_v7();
        let mut worker = processor(worker_id);
        let command =
            federated_skill_command(worker_id, "search", server_registered, scope_delegated);
        // The specific refusal, not just any error. Asserting is_err() alone
        // passes when the command is rejected for an unrelated reason, which is
        // exactly what happened while this test was being written.
        let error = worker
            .accept(command, chrono::Utc::now())
            .expect_err("must be refused");
        assert!(
            matches!(
                &error,
                agent_runtime_worker::WorkerAssignmentError::ToolConfiguration(message)
                    if message.contains("federated tool")
            ),
            "server_registered={server_registered} scope_delegated={scope_delegated}: \
             expected a federated-tool refusal, got {error:?}"
        );
    }
}

struct StubFederatedExecutor {
    digest: String,
}

fn attach_stub_catalog(
    worker: &mut WorkerProcessor,
    attempt_id: Uuid,
    digest: &str,
) -> Result<(), WorkerAssignmentError> {
    let definitions =
        federated_tool_definitions("search", digest, one_tool(), &BTreeMap::new()).unwrap();
    let mut registry = worker.tool_registry().clone();
    let mut executors: Vec<(String, std::sync::Arc<dyn agent_tool_runtime::ToolExecutor>)> =
        Vec::new();
    for definition in &definitions {
        registry.register(definition.descriptor.clone()).unwrap();
        executors.push((
            definition.descriptor.name.clone(),
            std::sync::Arc::new(StubFederatedExecutor {
                digest: digest.to_owned(),
            }),
        ));
    }
    worker.attach_federated_tools(
        attempt_id,
        registry,
        definitions,
        executors,
        agent_runtime_worker::McpDiscoveryPolicy::default(),
    )
}

impl agent_tool_runtime::ToolExecutor for StubFederatedExecutor {
    fn implementation_digest(&self) -> &str {
        &self.digest
    }

    fn execute(
        &self,
        _request: agent_protocol::ToolExecutionRequest,
        _context: agent_tool_runtime::ToolExecutionContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        agent_tool_runtime::ToolExecutionResult,
                        agent_tool_runtime::ToolExecutionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Ok(agent_tool_runtime::ToolExecutionResult {
                content: serde_json::json!([]),
                is_error: false,
                exit_code: 0,
            })
        })
    }
}

fn processor(worker_id: Uuid) -> WorkerProcessor {
    let mut worker = WorkerProcessor::new(
        worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_owned(),
    )
    .unwrap();
    worker.set_skill_artifact_verifier(agent_runtime_worker::SkillArtifactVerifier::new(
        // Must match the snapshot's signing_key_id, or verification fails
        // before the tool-name rule is ever reached.
        "local-skill-key",
        ed25519_dalek::SigningKey::from_bytes(&[91; 32]).verifying_key(),
    ));
    worker
}

fn federated_skill_command(
    worker_id: Uuid,
    server: &str,
    server_registered: bool,
    scope_delegated: bool,
) -> agent_protocol::RunExecutionCommand {
    use ed25519_dalek::Signer;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&[91; 32]);
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(9);
    // WorkerProcessor::new uses the worker id as its own incarnation, and accept
    // refuses a command addressed to another worker.
    value["worker_id"] = serde_json::json!(worker_id);
    value["worker_incarnation_id"] = serde_json::json!(worker_id);
    // The example is a recorded message; its lease expired long ago.
    value["issued_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    value["lease_expires_at"] =
        serde_json::json!((chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339());
    value["skill_snapshots"][0]["tool_names"] =
        serde_json::json!([format!("mcp:{server}/web_search")]);
    // tool_names is inside the artifact digest, so changing it means recomputing
    // the digest and re-signing. Using the file's original digest signs a
    // snapshot that no longer describes itself, and accept refuses it for a
    // reason that has nothing to do with what this test checks.
    let tenant_id: Uuid = value["tenant_id"].as_str().unwrap().parse().unwrap();
    let snapshot: agent_protocol::SkillSnapshot =
        serde_json::from_value(value["skill_snapshots"][0].clone()).unwrap();
    let digest = snapshot.expected_artifact_digest(tenant_id);
    value["skill_snapshots"][0]["artifact_digest"] = serde_json::json!(digest);
    let signature = signing_key.sign(format!("agent-runtime-skill-v1.{digest}").as_bytes());
    value["skill_snapshots"][0]["signature"] = serde_json::json!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    value["delegated_scopes"] = if scope_delegated {
        serde_json::json!([format!("tool:mcp:{server}")])
    } else {
        serde_json::json!(["tool:workspace.read"])
    };
    value["mcp_servers"] = if server_registered && scope_delegated {
        serde_json::json!([{
            "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
            "name": server,
            "endpoint": "https://mcp.example.com/rpc",
            "credential_envelope_base64": ""
        }])
    } else {
        serde_json::json!([])
    };
    serde_json::from_value(value).unwrap()
}

fn accepted_with_federated_skill(
    server: &str,
    server_registered: bool,
    scope_delegated: bool,
) -> (WorkerProcessor, agent_protocol::RunExecutionCommand) {
    let worker_id = Uuid::now_v7();
    let mut worker = processor(worker_id);
    let command = federated_skill_command(worker_id, server, server_registered, scope_delegated);
    worker
        .accept(command.clone(), chrono::Utc::now())
        .expect("a skill declaring a granted federated tool must be accepted");
    (worker, command)
}
