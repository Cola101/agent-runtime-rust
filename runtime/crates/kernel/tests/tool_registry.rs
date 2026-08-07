use agent_kernel::{RegistryError, ToolPlan, ToolRegistry};
use agent_protocol::{ApprovalMode, SandboxClass, ToolDescriptor, ToolEffect};
use serde_json::json;
use std::collections::BTreeSet;

fn shell_tool() -> ToolDescriptor {
    ToolDescriptor {
        name: "shell".into(),
        effect: ToolEffect::Unknown,
        approval: ApprovalMode::Ask,
        sandbox: SandboxClass::Kata,
        implementation_digest: "a".repeat(64),
        required_scopes: BTreeSet::from(["workspace:write".into()]),
    }
}

#[test]
fn tool_policy_binds_allow_and_ask_decisions_to_the_exact_call() {
    let mut registry = ToolRegistry::default();
    registry
        .register(ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Allow,
            sandbox: SandboxClass::RestrictedContainer,
            implementation_digest: "b".repeat(64),
            required_scopes: BTreeSet::from(["workspace:read".into()]),
        })
        .unwrap();
    registry
        .register(ToolDescriptor {
            name: "shell".into(),
            effect: ToolEffect::Unknown,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::Kata,
            implementation_digest: "a".repeat(64),
            required_scopes: BTreeSet::from(["workspace:write".into()]),
        })
        .unwrap();
    let scopes = BTreeSet::from(["workspace:read".into(), "workspace:write".into()]);

    let allowed = registry
        .plan(
            agent_protocol::ToolCall {
                id: "call_read".into(),
                name: "read_file".into(),
                arguments: json!({"path":"README.md"}),
            },
            &scopes,
        )
        .unwrap();
    let asked = registry
        .plan(
            agent_protocol::ToolCall {
                id: "call_shell".into(),
                name: "shell".into(),
                arguments: json!({"command":"cargo test"}),
            },
            &scopes,
        )
        .unwrap();

    let agent_kernel::ToolPlan::Execute(execution) = allowed else {
        panic!("allow policy must produce an execution request");
    };
    assert_eq!(execution.call.id, "call_read");
    assert_eq!(execution.effect, ToolEffect::Pure);
    assert_eq!(execution.sandbox, SandboxClass::RestrictedContainer);
    assert_eq!(execution.binding_digest.len(), 64);

    let agent_kernel::ToolPlan::ApprovalRequired(approval) = asked else {
        panic!("ask policy must produce a bound approval request");
    };
    assert_eq!(approval.execution.call.id, "call_shell");
    assert_eq!(approval.execution.effect, ToolEffect::Unknown);
    assert_eq!(approval.execution.sandbox, SandboxClass::Kata);
    assert_eq!(approval.execution.binding_digest.len(), 64);
}

#[test]
fn registry_rejects_duplicate_tool_names() {
    let mut registry = ToolRegistry::default();
    registry.register(shell_tool()).unwrap();

    let error = registry.register(shell_tool()).unwrap_err();

    assert_eq!(error, RegistryError::DuplicateTool("shell".into()));
}

#[test]
fn delegated_scope_must_cover_every_tool_scope() {
    let mut registry = ToolRegistry::default();
    registry.register(shell_tool()).unwrap();

    assert!(registry.authorize("shell", &BTreeSet::new()).is_err());
    assert!(
        registry
            .authorize("shell", &BTreeSet::from(["workspace:write".into()]))
            .is_ok()
    );
}

#[test]
fn approval_binding_changes_with_the_immutable_tool_implementation() {
    let call = agent_protocol::ToolCall {
        id: "call_read".into(),
        name: "read_file".into(),
        arguments: json!({"path":"README.md"}),
    };
    let mut first = ToolRegistry::default();
    first
        .register(ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::TrustedNative,
            implementation_digest: "1".repeat(64),
            required_scopes: BTreeSet::new(),
        })
        .unwrap();
    let mut second = ToolRegistry::default();
    second
        .register(ToolDescriptor {
            name: "read_file".into(),
            effect: ToolEffect::Pure,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::TrustedNative,
            implementation_digest: "2".repeat(64),
            required_scopes: BTreeSet::new(),
        })
        .unwrap();

    let agent_kernel::ToolPlan::ApprovalRequired(first) =
        first.plan(call.clone(), &BTreeSet::new()).unwrap()
    else {
        panic!("ask policy must produce an approval");
    };
    let agent_kernel::ToolPlan::ApprovalRequired(second) =
        second.plan(call, &BTreeSet::new()).unwrap()
    else {
        panic!("ask policy must produce an approval");
    };

    assert_ne!(
        first.execution.binding_digest,
        second.execution.binding_digest
    );
}

#[test]
fn session_approval_scope_ignores_call_id_but_binds_arguments_and_policy_snapshot() {
    let mut registry = ToolRegistry::default();
    registry.register(shell_tool()).unwrap();
    let scopes = BTreeSet::from(["workspace:write".into()]);
    let plan = |id: &str, command: &str| {
        let ToolPlan::ApprovalRequired(approval) = registry
            .plan(
                agent_protocol::ToolCall {
                    id: id.into(),
                    name: "shell".into(),
                    arguments: json!({"command":command}),
                },
                &scopes,
            )
            .unwrap()
        else {
            panic!("ask policy must produce an approval");
        };
        serde_json::to_value(approval).unwrap()
    };

    let first = plan("call_1", "cargo test");
    let repeated = plan("call_2", "cargo test");
    let changed_arguments = plan("call_3", "cargo clean");

    assert_ne!(
        first["execution"]["binding_digest"],
        repeated["execution"]["binding_digest"]
    );
    assert_eq!(
        first["session_scope_digest"],
        repeated["session_scope_digest"]
    );
    assert_ne!(
        first["session_scope_digest"],
        changed_arguments["session_scope_digest"]
    );
    assert_eq!(first["policy_snapshot"]["tool_name"], "shell");
    assert_eq!(first["policy_snapshot"]["effect"], "unknown");
    assert_eq!(first["policy_snapshot"]["approval"], "ask");
    assert_eq!(first["policy_snapshot"]["sandbox"], "kata");
    assert_eq!(
        first["policy_snapshot"]["implementation_digest"],
        "a".repeat(64)
    );
    assert_eq!(
        first["policy_snapshot"]["required_scopes"],
        json!(["workspace:write"])
    );
    assert!(
        first["policy_digest"]
            .as_str()
            .is_some_and(|digest| digest.len() == 64)
    );
}

#[test]
fn registry_rejects_a_tool_without_an_immutable_implementation_digest() {
    let mut descriptor = shell_tool();
    descriptor.implementation_digest = "latest".into();

    assert_eq!(
        ToolRegistry::default().register(descriptor).unwrap_err(),
        RegistryError::InvalidImplementationDigest
    );
}
