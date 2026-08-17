//! Containment must be a declared capability, not an assumption.
//!
//! Before ADR-0122 a non-macOS build ran a `TrustedNative` Tool with no
//! containment at all: `prepare` returned the bare executable and every record
//! — descriptor, implementation digest, approval — still said `TrustedNative`.
//! Nothing anywhere reported the difference.
//!
//! These tests prove the two halves of the fix on a host where containment IS
//! available, which is the only kind of host this repo can run on today:
//!
//! 1. A fabricated capability set with a missing guarantee is refused, and the
//!    error names the guarantee that was missing. This is the ADR-0072 pattern
//!    (`validate_governance` takes its capabilities as a parameter) precisely so
//!    the absent path is reachable from a host where it is not absent.
//! 2. The implementation digest distinguishes tools it previously could not.

use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ToolContainmentBackendKind, ToolContainmentCapabilities, ToolExecutionContext,
    ToolExecutionError, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
    validate_containment,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn temporary_directory(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("agent-containment-{label}-"))
        .tempdir()
        .unwrap()
}

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("trusted-tool");
    fs::write(&executable, "#!/bin/sh\nset -eu\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn definition(
    trusted_root: &Path,
    executable: &Path,
    workspace_access: WorkspaceAccess,
) -> TrustedNativeToolDefinition {
    TrustedNativeToolDefinition {
        trusted_root: trusted_root.to_path_buf(),
        executable: executable.to_path_buf(),
        fixed_args: vec!["--stdio".into()],
        workspace_access,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 16 * 1024,
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_containment_1".into(),
            name: "workspace.read_text".into(),
            arguments: json!({"path":"README.txt"}),
        },
        effect: ToolEffect::Pure,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "b".repeat(64),
    }
}

fn context(workspace_root: PathBuf) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        workspace_root,
        timeout: Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

fn contained() -> ToolContainmentCapabilities {
    ToolContainmentCapabilities {
        backend: ToolContainmentBackendKind::MacosSeatbelt,
        workspace_write_confinement: true,
        credential_read_denial: true,
        network_egress_denial: true,
    }
}

#[test]
fn a_host_without_a_containment_backend_is_refused() {
    let capabilities = ToolContainmentCapabilities {
        backend: ToolContainmentBackendKind::Unsupported,
        workspace_write_confinement: false,
        credential_read_denial: false,
        network_egress_denial: false,
    };

    assert_eq!(
        validate_containment(capabilities),
        Err(ToolExecutionError::UnsupportedContainment(
            "containment_backend"
        )),
        "an unsupported backend must be refused, not silently skipped"
    );
}

/// The refusal is per guarantee, not merely "is there a backend".
///
/// A future backend that exists but cannot close one of these boundaries must
/// be refused for *that* boundary, so the caller learns which promise the host
/// cannot keep rather than a generic failure.
#[test]
fn each_missing_guarantee_is_named_individually() {
    for (mutate, expected) in [
        (
            (|c: &mut ToolContainmentCapabilities| c.workspace_write_confinement = false)
                as fn(&mut ToolContainmentCapabilities),
            "workspace_write_confinement",
        ),
        (
            |c: &mut ToolContainmentCapabilities| c.credential_read_denial = false,
            "credential_read_denial",
        ),
        (
            |c: &mut ToolContainmentCapabilities| c.network_egress_denial = false,
            "network_egress_denial",
        ),
    ] {
        let mut capabilities = contained();
        mutate(&mut capabilities);

        assert_eq!(
            validate_containment(capabilities),
            Err(ToolExecutionError::UnsupportedContainment(expected)),
            "a backend missing {expected} must be refused by that name"
        );
    }
}

#[test]
fn a_fully_contained_capability_set_is_accepted() {
    assert_eq!(validate_containment(contained()), Ok(()));
}

/// Guards the `cfg!`-derived vector against drifting from what the launch path
/// is compiled to do. If this build lost Seatbelt, `prepare` below would refuse
/// and this assertion is how we would know why.
#[cfg(target_os = "macos")]
#[test]
fn this_host_declares_seatbelt_containment() {
    let capabilities = TrustedNativeExecutor::containment_capabilities();

    assert_eq!(
        capabilities.backend,
        ToolContainmentBackendKind::MacosSeatbelt
    );
    assert!(capabilities.workspace_write_confinement);
    assert!(capabilities.credential_read_denial);
    assert!(capabilities.network_egress_denial);
    assert_eq!(validate_containment(capabilities), Ok(()));
}

/// The declared capability and the actual launch must agree.
///
/// Declaring containment while launching bare would be the same defect wearing
/// a capability vector, so this asserts the launch really is wrapped.
#[cfg(target_os = "macos")]
#[test]
fn a_declared_backend_actually_wraps_the_launch() {
    let trusted_root = temporary_directory("wrapped");
    let executable = executable_script(trusted_root.path());
    let workspace = temporary_directory("wrapped-ws");
    let executor = TrustedNativeExecutor::new(definition(
        trusted_root.path(),
        &executable,
        WorkspaceAccess::ReadOnly,
    ))
    .unwrap();

    let launch = executor
        .prepare(&request(), &context(workspace.path().to_path_buf()))
        .unwrap();

    assert!(
        launch.program.ends_with("sandbox-exec"),
        "a host declaring MacosSeatbelt must launch through it, got {:?}",
        launch.program
    );
    assert!(
        launch.args.iter().any(|arg| arg == "-p"),
        "the launch must carry a Seatbelt profile"
    );
}

/// Regression: `workspace_access` was hardcoded to "read_only" in the digest
/// while the launch path honoured the real value, so a Tool that could write
/// the Workspace and one that could not were indistinguishable to every
/// consumer that compares implementation digests.
#[test]
fn read_only_and_read_write_tools_do_not_share_an_implementation_digest() {
    let trusted_root = temporary_directory("digest");
    let executable = executable_script(trusted_root.path());

    let read_only = TrustedNativeExecutor::new(definition(
        trusted_root.path(),
        &executable,
        WorkspaceAccess::ReadOnly,
    ))
    .unwrap();
    let read_write = TrustedNativeExecutor::new(definition(
        trusted_root.path(),
        &executable,
        WorkspaceAccess::ReadWrite,
    ))
    .unwrap();

    assert_ne!(
        read_only.implementation_digest(),
        read_write.implementation_digest(),
        "identical binary and args must not hide a different Workspace boundary"
    );
}

/// A refusal that happened before anything was spawned must not be reported as
/// "it might have run".
///
/// `record_tool_execution_failure` sends any *unclassified* failure of a
/// `NonIdempotent`/`Unknown` Tool to `run.indeterminate`, which is the branch
/// that asks a human to go and check whether an effect landed. For this error
/// there is nothing to check: the Workspace was never resolved and no process
/// was created.
#[test]
fn an_uncontained_host_is_a_deterministic_failure_not_an_ambiguous_one() {
    let error = ToolExecutionError::UnsupportedContainment("workspace_write_confinement");

    let result = error.deterministic_failure_result().expect(
        "a refusal made before any process is created never crossed a side-effect boundary",
    );

    assert!(result.is_error);
    assert_eq!(
        result.content["error"]["code"], "tool_containment_unsupported",
        "the model needs a stable code it can reason about"
    );
    assert!(
        !result
            .content
            .to_string()
            .contains("workspace_write_confinement"),
        "the missing guarantee is operator-facing; it must not reach the model"
    );
}

#[test]
fn the_same_definition_still_digests_identically() {
    let trusted_root = temporary_directory("stable");
    let executable = executable_script(trusted_root.path());

    let first = TrustedNativeExecutor::new(definition(
        trusted_root.path(),
        &executable,
        WorkspaceAccess::ReadWrite,
    ))
    .unwrap();
    let second = TrustedNativeExecutor::new(definition(
        trusted_root.path(),
        &executable,
        WorkspaceAccess::ReadWrite,
    ))
    .unwrap();

    assert_eq!(
        first.implementation_digest(),
        second.implementation_digest(),
        "the digest must stay a function of the definition alone"
    );
}
