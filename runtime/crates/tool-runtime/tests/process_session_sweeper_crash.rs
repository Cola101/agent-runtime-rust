use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionGovernance, ProcessSessionInteraction, ProcessSessionStartRequest,
    ProcessSessionState, ProcessSessionTerminationReason, ToolExecutionContext,
    TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("swept-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         printf 'ready\\n'\n\
         while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done\n",
    )
    .unwrap();
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

fn governance() -> ProcessSessionGovernance {
    ProcessSessionGovernance {
        max_runtime: Duration::from_millis(200),
        idle_timeout: Duration::from_secs(5),
        ..ProcessSessionGovernance::default()
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "sweeper_crash_owner_start".into(),
            name: "process.start".into(),
            arguments: json!({}),
        },
        effect: ToolEffect::NonIdempotent,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "d".repeat(64),
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

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks the child identity recorded by the owner.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[tokio::test(flavor = "current_thread")]
async fn replacement_sweeper_enforces_the_original_deadline_after_owner_crash() {
    const OWNER_MODE: &str = "AGENT_PROCESS_SESSION_SWEEPER_OWNER";
    const TEST_NAME: &str = "replacement_sweeper_enforces_the_original_deadline_after_owner_crash";

    if std::env::var_os(OWNER_MODE).is_some() {
        let state_root = PathBuf::from(std::env::var_os("AGENT_PROCESS_SESSION_STATE").unwrap());
        let trusted_root =
            PathBuf::from(std::env::var_os("AGENT_PROCESS_SESSION_TRUSTED").unwrap());
        let workspace_root =
            PathBuf::from(std::env::var_os("AGENT_PROCESS_SESSION_WORKSPACE").unwrap());
        let executable =
            PathBuf::from(std::env::var_os("AGENT_PROCESS_SESSION_EXECUTABLE").unwrap());
        let tenant_id = std::env::var("AGENT_PROCESS_SESSION_TENANT")
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let session_id = std::env::var("AGENT_PROCESS_SESSION_ID")
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let manager = PersistentProcessSessionManager::new_with_governance(
            state_root,
            executor(&trusted_root, &executable),
            16 * 1024,
            governance(),
        )
        .unwrap();
        let workspace_root = workspace_root.canonicalize().unwrap();
        let started = manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, workspace_root),
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap();
        fs::write(
            std::env::var_os("AGENT_PROCESS_SESSION_HANDOFF").unwrap(),
            started.pid.unwrap().to_string(),
        )
        .unwrap();
        std::process::exit(73);
    }

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let handoff = state.path().join("owner-handoff");
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(OWNER_MODE, "1")
        .env("AGENT_PROCESS_SESSION_STATE", state.path())
        .env("AGENT_PROCESS_SESSION_TRUSTED", trusted.path())
        .env("AGENT_PROCESS_SESSION_WORKSPACE", workspace.path())
        .env("AGENT_PROCESS_SESSION_EXECUTABLE", &executable)
        .env("AGENT_PROCESS_SESSION_TENANT", tenant_id.to_string())
        .env("AGENT_PROCESS_SESSION_ID", session_id.to_string())
        .env("AGENT_PROCESS_SESSION_HANDOFF", &handoff)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let original_pid = fs::read_to_string(&handoff)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(process_alive(original_pid));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let replacement = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance(),
    )
    .unwrap();
    let report = replacement.sweep().await.unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let output = replacement
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
    if output.state == ProcessSessionState::Running {
        let _ = replacement
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: output.stdout_cursor,
                    stderr_cursor: output.stderr_cursor,
                    action: ProcessSessionAction::Close,
                },
            )
            .await;
    }

    assert_eq!(report.examined, 1);
    assert_eq!(report.terminated, 1);
    assert_eq!(report.active, 0);
    assert_eq!(report.indeterminate, 0);
    assert_eq!(output.state, ProcessSessionState::Terminated);
    assert_eq!(
        output.termination_reason,
        Some(ProcessSessionTerminationReason::ExecutionDeadline)
    );
    assert!(!process_alive(original_pid));
}
