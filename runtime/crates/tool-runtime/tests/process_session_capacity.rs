use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionInteraction, ProcessSessionStartRequest, ProcessSessionState,
    ToolExecutionContext, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("one-shot-session");
    fs::write(&executable, "#!/bin/sh\nprintf 'done\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn executor(root: &Path, executable: &Path) -> TrustedNativeExecutor {
    TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: root.to_path_buf(),
        executable: executable.to_path_buf(),
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 16 * 1024,
    })
    .unwrap()
}

fn request(ordinal: usize) -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: format!("capacity_{ordinal}"),
            name: "process.start".into(),
            arguments: json!({}),
        },
        effect: ToolEffect::NonIdempotent,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "c".repeat(64),
    }
}

fn context(tenant_id: Uuid, workspace_root: PathBuf) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id,
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

#[tokio::test]
async fn terminal_session_history_does_not_exhaust_the_live_process_capacity() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };

    for ordinal in 0..65 {
        let session_id = Uuid::now_v7();
        manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: request(ordinal),
                context: context(tenant_id, workspace_root.clone()),
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap_or_else(|error| panic!("session {ordinal} was rejected: {error}"));
        let mut terminal = false;
        for _ in 0..500 {
            let output = manager
                .interact(
                    &access,
                    ProcessSessionInteraction {
                        session_id,
                        stdout_cursor: 0,
                        stderr_cursor: 0,
                        action: ProcessSessionAction::Poll,
                    },
                )
                .await
                .unwrap();
            terminal = matches!(
                output.state,
                ProcessSessionState::Exited | ProcessSessionState::Terminated
            );
            if terminal {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(terminal, "session {ordinal} never became terminal");
    }
}
