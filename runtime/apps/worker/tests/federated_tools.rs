//! Federated MCP tools joining the kernel's Tool registry (ADR-0040).

use agent_kernel::ToolPlan;
use agent_protocol::{ApprovalMode, AutoApproval, SandboxClass, ToolCall, ToolEffect};
use agent_runtime_worker::{federated_tool_definitions, WorkerProcessor};
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
