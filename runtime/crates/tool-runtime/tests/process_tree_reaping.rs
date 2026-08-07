//! A timeout must end the whole process tree, not just the process we spawned.
//!
//! Before `shell.exec` the direct child *was* the whole tree: one registered
//! binary that read JSON and exited. A shell command is
//! `sandbox-exec -> tool -> /bin/sh -c -> whatever the model wrote`, and
//! `Child::kill` sends a signal to one pid. Everything the command started
//! behind that pid survived the timeout, kept its Workspace handles, and kept
//! running with no owner and nothing watching it.

#![cfg(unix)]

use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, TrustedNativeExecutor, TrustedNativeToolDefinition,
    WorkspaceAccess,
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
        .prefix(&format!("agent-reap-{label}-"))
        .tempdir()
        .unwrap()
}

/// Stands in for the trusted tool: it starts a grandchild that outlives it and
/// then blocks, which is exactly the shape `shell.exec` produces when a command
/// backgrounds something.
fn spawning_script(root: &Path, marker: &Path) -> PathBuf {
    let executable = root.join("spawns-a-grandchild");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\n\
             ( while : ; do /usr/bin/touch '{}' 2>/dev/null; /bin/sleep 0.05; done ) &\n\
             /bin/sleep 60\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn definition(trusted_root: &Path, executable: &Path) -> TrustedNativeToolDefinition {
    TrustedNativeToolDefinition {
        trusted_root: trusted_root.to_path_buf(),
        executable: executable.to_path_buf(),
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_reap_1".into(),
            name: "shell.exec".into(),
            arguments: json!({"command": "sleep 60 &"}),
        },
        effect: ToolEffect::NonIdempotent,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "d".repeat(64),
    }
}

fn context(workspace_root: PathBuf, timeout: Duration) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        workspace_root,
        timeout,
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

/// The marker file is touched every 50ms by the grandchild. If reaping worked,
/// its mtime stops advancing once the Tool times out.
async fn marker_still_advancing(marker: &Path) -> bool {
    let first = fs::metadata(marker)
        .ok()
        .and_then(|meta| meta.modified().ok());
    tokio::time::sleep(Duration::from_millis(600)).await;
    let second = fs::metadata(marker)
        .ok()
        .and_then(|meta| meta.modified().ok());
    match (first, second) {
        (Some(first), Some(second)) => second > first,
        _ => false,
    }
}

#[tokio::test]
async fn a_timeout_reaps_the_whole_process_tree_not_just_the_direct_child() {
    let root = temporary_directory("timeout");
    let workspace = temporary_directory("timeout-ws");
    let marker = workspace.path().join("grandchild-alive");
    let executable = spawning_script(root.path(), &marker);

    let executor = TrustedNativeExecutor::new(definition(root.path(), &executable)).unwrap();
    let outcome = executor
        .execute(
            request(),
            context(workspace.path().to_path_buf(), Duration::from_millis(2000)),
        )
        .await;

    assert!(
        matches!(outcome, Err(ToolExecutionError::TimedOut)),
        "expected a timeout, got {outcome:?}"
    );
    // Without this the next check would pass because nothing ever ran.
    assert!(
        marker.exists(),
        "the grandchild never started, so this proves nothing about reaping"
    );
    assert!(
        !marker_still_advancing(&marker).await,
        "a background process survived the Tool timeout and is still running unowned"
    );
}

#[tokio::test]
async fn a_cancellation_reaps_the_whole_process_tree() {
    let root = temporary_directory("cancel");
    let workspace = temporary_directory("cancel-ws");
    let marker = workspace.path().join("grandchild-alive");
    let executable = spawning_script(root.path(), &marker);

    let executor = TrustedNativeExecutor::new(definition(root.path(), &executable)).unwrap();
    let mut execution_context = context(workspace.path().to_path_buf(), Duration::from_secs(30));
    let cancellation = execution_context.cancellation.clone();
    execution_context.cancellation = cancellation.clone();
    // Long enough for the grandchild to have started touching. A shorter delay
    // cancels before it exists, and the guard below then (correctly) reports
    // that the test proved nothing.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(2000)).await;
        cancellation.cancel();
    });

    let outcome = executor.execute(request(), execution_context).await;

    assert!(
        matches!(outcome, Err(ToolExecutionError::Cancelled)),
        "expected a cancellation, got {outcome:?}"
    );
    assert!(
        marker.exists(),
        "the grandchild never started, so this proves nothing about reaping"
    );
    assert!(
        !marker_still_advancing(&marker).await,
        "a background process survived cancellation and is still running unowned"
    );
}
