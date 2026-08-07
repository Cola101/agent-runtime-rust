//! ADR-0037 left one gap: the credential denials were built from `$HOME`, and
//! with `$HOME` unset the launch proceeded with no denials at all and reported
//! nothing. That is the same shape as the bug found while implementing it -- a
//! container that reads as protected and is not.
//!
//! This file holds exactly one test on purpose. It mutates process environment,
//! which is unsound to do while other threads read it; Cargo gives each
//! integration test file its own process, so a single test here has no
//! concurrent reader.

#![cfg(target_os = "macos")]

use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ToolExecutionContext, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Returns a guard, not a path: dropping it removes the directory.
fn temporary_directory(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("agent-no-home-{label}-"))
        .tempdir()
        .unwrap()
}

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("trusted-tool");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

#[test]
fn credential_denials_survive_an_unset_home_variable() {
    let trusted_root = temporary_directory("root");
    let executable = executable_script(trusted_root.path());
    let workspace = temporary_directory("workspace");

    // Single-test file, so nothing else in this process reads the environment
    // concurrently.
    unsafe { std::env::remove_var("HOME") };

    let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: trusted_root.path().to_path_buf(),
        executable: executable.clone(),
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadOnly,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    })
    .unwrap();

    let launch = executor
        .prepare(
            &ToolExecutionRequest {
                call: ToolCall {
                    id: "call_no_home".into(),
                    name: "workspace.read_text".into(),
                    arguments: json!({ "path": "README.txt" }),
                },
                effect: ToolEffect::Pure,
                sandbox: SandboxClass::TrustedNative,
                binding_digest: "b".repeat(64),
            },
            &ToolExecutionContext {
                tenant_id: Uuid::now_v7(),
                run_id: Uuid::now_v7(),
                attempt_id: Uuid::now_v7(),
                workspace_root: workspace.path().to_path_buf(),
                timeout: Duration::from_secs(10),
                cancellation: CancellationToken::new(),
                requested_at: Utc::now(),
            },
        )
        .expect("a resolvable home directory must not fail the launch");

    let denied = launch
        .args
        .iter()
        .filter(|argument| argument.starts_with("AGENT_RUNTIME_DENIED_READ_"))
        .count();
    assert!(
        denied > 0,
        "containment silently degraded to no credential denials with $HOME unset; args = {:?}",
        launch.args
    );

    let profile = &launch.args[1];
    assert!(
        profile.contains("(deny file-read*"),
        "the profile carries no read denial: {profile}"
    );
}
