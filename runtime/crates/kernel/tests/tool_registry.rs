use agent_kernel::{RegistryError, ToolPlan, ToolRegistry};
use agent_protocol::{
    ApprovalMode, AutoApproval, SandboxClass, ToolCall, ToolDescriptor, ToolEffect,
};
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
        auto_approval: AutoApproval::Never,
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
            auto_approval: AutoApproval::Never,
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
            auto_approval: AutoApproval::Never,
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
            auto_approval: AutoApproval::Never,
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
            auto_approval: AutoApproval::Never,
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

/// The exemption must come from the Tool's declared policy, never from the
/// command text alone. A Worker that decided this on its own would be bypassing
/// the control plane's approval ledger, which is the thing the gate exists to
/// keep honest.
#[test]
fn a_read_only_command_is_exempt_only_when_the_tool_declares_the_policy() {
    fn plan_for(auto_approval: AutoApproval, command: &str) -> ToolPlan {
        let mut registry = ToolRegistry::default();
        registry
            .register(ToolDescriptor {
                name: "shell.exec".into(),
                effect: ToolEffect::NonIdempotent,
                approval: ApprovalMode::Ask,
                sandbox: SandboxClass::TrustedNative,
                implementation_digest: "c".repeat(64),
                required_scopes: BTreeSet::from(["tool:shell.exec".into()]),
                auto_approval,
            })
            .unwrap();
        registry
            .plan(
                ToolCall {
                    id: "call_1".into(),
                    name: "shell.exec".into(),
                    arguments: serde_json::json!({ "command": command }),
                },
                &BTreeSet::from(["tool:shell.exec".to_string()]),
            )
            .unwrap()
    }

    // Same command, two policies: only the declaring Tool skips the gate, and
    // it lands in AutoApproved rather than Execute so the exemption stays
    // distinguishable in the durable log.
    assert!(matches!(
        plan_for(AutoApproval::ProvablyReadOnlyShellCommand, "ls -la"),
        ToolPlan::AutoApproved { .. }
    ));
    assert!(matches!(
        plan_for(AutoApproval::Never, "ls -la"),
        ToolPlan::ApprovalRequired(_)
    ));

    // Declaring the policy exempts nothing that is not provably read-only.
    assert!(matches!(
        plan_for(AutoApproval::ProvablyReadOnlyShellCommand, "rm -rf /"),
        ToolPlan::ApprovalRequired(_)
    ));
    assert!(matches!(
        plan_for(AutoApproval::ProvablyReadOnlyShellCommand, "ls; rm -rf /"),
        ToolPlan::ApprovalRequired(_)
    ));
}

/// The ledger has to be able to show that an exemption was in force. A snapshot
/// that omitted it would make an auto-approved call indistinguishable from a
/// Tool that was never approval gated at all.
#[test]
fn the_policy_snapshot_records_the_exemption() {
    let mut registry = ToolRegistry::default();
    registry
        .register(ToolDescriptor {
            name: "shell.exec".into(),
            effect: ToolEffect::NonIdempotent,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::TrustedNative,
            implementation_digest: "c".repeat(64),
            required_scopes: BTreeSet::from(["tool:shell.exec".into()]),
            auto_approval: AutoApproval::ProvablyReadOnlyShellCommand,
        })
        .unwrap();

    let ToolPlan::ApprovalRequired(request) = registry
        .plan(
            ToolCall {
                id: "call_1".into(),
                name: "shell.exec".into(),
                arguments: serde_json::json!({ "command": "rm -rf /" }),
            },
            &BTreeSet::from(["tool:shell.exec".to_string()]),
        )
        .unwrap()
    else {
        panic!("a command that is not read-only must still be approval gated");
    };
    assert_eq!(
        request.policy_snapshot.expect("snapshot").auto_approval,
        AutoApproval::ProvablyReadOnlyShellCommand
    );
}

/// The binding digest must cover the policy that produced the decision.
///
/// It did not. A call that was approval gated and the same call that was
/// exempted produced identical digests, so a decision taken under one policy
/// bound just as well to an execution under another, and the ledger could not
/// tell them apart by digest at all.
#[test]
fn the_binding_digest_covers_the_approval_policy_not_only_the_call() {
    fn digest_for(approval: ApprovalMode, auto_approval: AutoApproval) -> String {
        let mut registry = ToolRegistry::default();
        registry
            .register(ToolDescriptor {
                name: "shell.exec".into(),
                effect: ToolEffect::NonIdempotent,
                approval,
                sandbox: SandboxClass::TrustedNative,
                implementation_digest: "c".repeat(64),
                required_scopes: BTreeSet::from(["tool:shell.exec".into()]),
                auto_approval,
            })
            .unwrap();
        let plan = registry
            .plan(
                ToolCall {
                    id: "call_1".into(),
                    name: "shell.exec".into(),
                    arguments: json!({ "command": "ls" }),
                },
                &BTreeSet::from(["tool:shell.exec".to_string()]),
            )
            .unwrap();
        match plan {
            ToolPlan::Execute(execution) | ToolPlan::Denied(execution) => execution.binding_digest,
            ToolPlan::AutoApproved { execution, .. } => execution.binding_digest,
            ToolPlan::ApprovalRequired(request) => request.execution.binding_digest,
            ToolPlan::SubagentSpawn(_) => panic!("shell.exec does not spawn a subagent"),
        }
    }

    let gated = digest_for(ApprovalMode::Ask, AutoApproval::Never);
    let allowed = digest_for(ApprovalMode::Allow, AutoApproval::Never);
    let exempted = digest_for(
        ApprovalMode::Ask,
        AutoApproval::ProvablyReadOnlyShellCommand,
    );

    assert_ne!(gated, allowed, "approval mode must change the binding");
    assert_ne!(
        gated, exempted,
        "an exempted call must not share a binding digest with a gated one"
    );
}

/// An auto-approved execution has to carry why it was auto-approved. Returning a
/// bare execution meant the policy snapshot and digest were computed and then
/// dropped, so nothing downstream could persist the reason -- which made
/// ADR-0039's claim that the exemption stays auditable untrue in the code.
#[test]
fn an_auto_approved_plan_carries_the_policy_that_exempted_it() {
    let mut registry = ToolRegistry::default();
    registry
        .register(ToolDescriptor {
            name: "shell.exec".into(),
            effect: ToolEffect::NonIdempotent,
            approval: ApprovalMode::Ask,
            sandbox: SandboxClass::TrustedNative,
            implementation_digest: "c".repeat(64),
            required_scopes: BTreeSet::from(["tool:shell.exec".into()]),
            auto_approval: AutoApproval::ProvablyReadOnlyShellCommand,
        })
        .unwrap();

    let plan = registry
        .plan(
            ToolCall {
                id: "call_1".into(),
                name: "shell.exec".into(),
                arguments: json!({ "command": "ls" }),
            },
            &BTreeSet::from(["tool:shell.exec".to_string()]),
        )
        .unwrap();

    let ToolPlan::AutoApproved {
        policy_snapshot,
        policy_digest,
        reason,
        ..
    } = plan
    else {
        panic!("an exempted call must be distinguishable from an ungated Allow: {plan:?}");
    };
    assert_eq!(
        policy_snapshot.auto_approval,
        AutoApproval::ProvablyReadOnlyShellCommand
    );
    assert_eq!(policy_digest.len(), 64);
    assert!(!reason.is_empty(), "the ledger needs a stated reason");
}
