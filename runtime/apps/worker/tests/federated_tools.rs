//! Federated MCP tools joining the kernel's Tool registry (ADR-0040).

use agent_kernel::ToolPlan;
use agent_protocol::{ApprovalMode, AutoApproval, SandboxClass, ToolCall, ToolEffect};
use agent_runtime_worker::{federated_tool_definitions, WorkerProcessor};
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
    let definitions = federated_tool_definitions("search", &catalog_digest(), one_tool()).unwrap();

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
    for definition in federated_tool_definitions("search", &catalog_digest(), one_tool()).unwrap() {
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

    assert!(federated_tool_definitions("search", &catalog_digest(), hostile).is_err());
}

/// Without a real catalog digest a Checkpoint restore has nothing to recompute,
/// so the registration is refused rather than given a placeholder.
#[test]
fn a_catalog_digest_that_is_not_a_digest_is_refused() {
    for bad in ["", "not-a-digest", &"z".repeat(64)] {
        assert!(
            federated_tool_definitions("search", bad, one_tool()).is_err(),
            "digest {bad:?} must be refused"
        );
    }
}

const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");

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
    let definitions = federated_tool_definitions("search", &catalog_digest(), one_tool()).unwrap();
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
        .attach_federated_tools(attempt_id, registry, definitions, executors)
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
    worker.set_skill_artifact_verifier(
        agent_runtime_worker::SkillArtifactVerifier::new(
            // Must match the snapshot's signing_key_id, or verification fails
            // before the tool-name rule is ever reached.
            "local-skill-key",
            ed25519_dalek::SigningKey::from_bytes(&[91; 32]).verifying_key(),
        ),
    );
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
