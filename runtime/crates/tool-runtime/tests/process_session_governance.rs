use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionError, ProcessSessionGovernance, ProcessSessionInteraction,
    ProcessSessionPtySupervisorConfig, ProcessSessionQuotaScope, ProcessSessionRecovery,
    ProcessSessionStartRequest, ProcessSessionState, ProcessSessionTerminationReason,
    ToolExecutionContext, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("governed-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         printf 'ready\\n'\n\
         while IFS= read -r line; do\n\
           printf 'got:%s\\n' \"$line\"\n\
         done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn noisy_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("noisy-governed-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         while :; do\n\
           printf '0123456789abcdef0123456789abcdef\\n'\n\
         done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn limit_reporting_script(root: &Path) -> PathBuf {
    let executable = root.join("limit-reporting-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         printf 'cpu:%s\\n' \"$(ulimit -t)\"\n\
         if [ \"$(uname -s)\" = Darwin ]; then\n\
           printf 'memory-kib:%s\\n' \"$(ulimit -d)\"\n\
         else\n\
           printf 'memory-kib:%s\\n' \"$(ulimit -v)\"\n\
         fi\n\
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

fn request(call_id: &str) -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: call_id.into(),
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
    // SAFETY: signal 0 does not mutate the process and the pid came from the
    // session manager.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn close_if_running(
    manager: &PersistentProcessSessionManager,
    access: &ProcessSessionAccess,
    session_id: Uuid,
) {
    let _ = manager
        .interact(
            access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Close,
            },
        )
        .await;
}

#[tokio::test]
async fn execution_deadline_terminates_the_process_group_without_a_poll() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_runtime: Duration::from_millis(150),
        idle_timeout: Duration::from_secs(5),
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let session_id = Uuid::now_v7();
    let started = manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("deadline-start"),
            context: context(tenant_id, workspace_root),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let original_pid = started.pid.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !process_alive(original_pid),
        "the deadline supervisor left pid {original_pid} alive"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let output = loop {
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
        if matches!(
            output.state,
            ProcessSessionState::Exited
                | ProcessSessionState::Terminated
                | ProcessSessionState::Indeterminate
        ) || Instant::now() >= deadline
        {
            break output;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    if !matches!(
        output.state,
        ProcessSessionState::Exited
            | ProcessSessionState::Terminated
            | ProcessSessionState::Indeterminate
    ) {
        close_if_running(&manager, &access, session_id).await;
    }

    let manifest = fs::read_to_string(
        state
            .path()
            .join("process-sessions")
            .join(session_id.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    assert_eq!(
        output.state,
        ProcessSessionState::Terminated,
        "supervised PTY did not converge after its output limit: {manifest}"
    );
    assert_eq!(
        output.termination_reason,
        Some(ProcessSessionTerminationReason::ExecutionDeadline)
    );
    assert!(output.pid.is_none());
}

#[tokio::test]
async fn tenant_quota_counts_live_sessions_across_workspaces() {
    let state = tempfile::tempdir().unwrap();
    let workspace_a = tempfile::tempdir().unwrap();
    let workspace_b = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_active_sessions: 4,
        max_active_sessions_per_tenant: 1,
        max_active_sessions_per_workspace: 1,
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_a = workspace_a.path().canonicalize().unwrap();
    let workspace_b = workspace_b.path().canonicalize().unwrap();
    let session_id = Uuid::now_v7();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("tenant-first"),
            context: context(tenant_id, workspace_a.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let second_session_id = Uuid::now_v7();
    let second_access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_b.clone(),
    };
    let result = manager
        .start(ProcessSessionStartRequest {
            session_id: second_session_id,
            request: request("tenant-second"),
            context: context(tenant_id, workspace_b),
            initial_stdin: Vec::new(),
        })
        .await;
    if result.is_ok() {
        close_if_running(&manager, &second_access, second_session_id).await;
    }
    close_if_running(
        &manager,
        &ProcessSessionAccess {
            tenant_id,
            workspace_root: workspace_a,
        },
        session_id,
    )
    .await;

    assert_eq!(
        result.unwrap_err(),
        ProcessSessionError::QuotaExceeded(ProcessSessionQuotaScope::Tenant)
    );
}

#[tokio::test]
async fn workspace_quota_is_scoped_by_tenant_and_canonical_workspace() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_active_sessions: 4,
        max_active_sessions_per_tenant: 4,
        max_active_sessions_per_workspace: 1,
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let session_id = Uuid::now_v7();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("workspace-first"),
            context: context(tenant_id, workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let second_session_id = Uuid::now_v7();
    let result = manager
        .start(ProcessSessionStartRequest {
            session_id: second_session_id,
            request: request("workspace-second"),
            context: context(tenant_id, workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await;
    if result.is_ok() {
        close_if_running(
            &manager,
            &ProcessSessionAccess {
                tenant_id,
                workspace_root: workspace_root.clone(),
            },
            second_session_id,
        )
        .await;
    }
    close_if_running(
        &manager,
        &ProcessSessionAccess {
            tenant_id,
            workspace_root,
        },
        session_id,
    )
    .await;

    assert_eq!(
        result.unwrap_err(),
        ProcessSessionError::QuotaExceeded(ProcessSessionQuotaScope::Workspace)
    );
}

#[tokio::test]
async fn stdin_activity_resets_idle_timeout_but_polling_does_not() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_runtime: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(1),
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let session_id = Uuid::now_v7();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("idle-start"),
            context: context(tenant_id, workspace_root),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(250)).await;
    let written = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Write {
                    bytes: b"still active\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let still_running = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: written.stdout_cursor,
                stderr_cursor: written.stderr_cursor,
                action: ProcessSessionAction::Poll,
            },
        )
        .await
        .unwrap();
    assert_eq!(still_running.state, ProcessSessionState::Running);

    let deadline = Instant::now() + Duration::from_secs(3);
    let terminated = loop {
        let output = manager
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: still_running.stdout_cursor,
                    stderr_cursor: still_running.stderr_cursor,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap();
        if output.state == ProcessSessionState::Terminated || Instant::now() >= deadline {
            break output;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    if terminated.state != ProcessSessionState::Terminated {
        close_if_running(&manager, &access, session_id).await;
    }

    assert_eq!(terminated.state, ProcessSessionState::Terminated);
    assert_eq!(
        terminated.termination_reason,
        Some(ProcessSessionTerminationReason::IdleTimeout)
    );
}

#[tokio::test]
async fn output_budget_is_a_hard_per_stream_file_limit() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = noisy_executable_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_runtime: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        max_output_bytes_per_stream: 4 * 1024,
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let session_id = Uuid::now_v7();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("output-start"),
            context: context(tenant_id, workspace_root),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let output = loop {
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
        if output.state == ProcessSessionState::Terminated || Instant::now() >= deadline {
            break output;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if output.state != ProcessSessionState::Terminated {
        close_if_running(&manager, &access, session_id).await;
    }
    let stdout_len = fs::metadata(
        state
            .path()
            .join("process-sessions")
            .join(session_id.to_string())
            .join("stdout.log"),
    )
    .unwrap()
    .len();

    assert_eq!(output.state, ProcessSessionState::Terminated);
    assert_eq!(
        output.termination_reason,
        Some(ProcessSessionTerminationReason::OutputLimit)
    );
    assert!(stdout_len <= 4 * 1024, "stdout grew to {stdout_len} bytes");
}

#[tokio::test]
async fn supervised_pty_output_budget_stays_bounded_after_host_independence() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = noisy_executable_script(trusted.path());
    let output_limit = 4 * 1024;
    let governance = ProcessSessionGovernance {
        max_runtime: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        max_output_bytes_per_stream: output_limit,
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        1024,
        governance,
        Some(ProcessSessionPtySupervisorConfig {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_agent-pty-session-supervisor")),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(5),
        }),
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let session_id = Uuid::now_v7();
    manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request("supervised-output-start"),
                context: context(tenant_id, workspace_root),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let output = loop {
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
        if output.state == ProcessSessionState::Terminated || Instant::now() >= deadline {
            break output;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    if output.state != ProcessSessionState::Terminated {
        close_if_running(&manager, &access, session_id).await;
    }
    let stdout_len = fs::metadata(
        state
            .path()
            .join("process-sessions")
            .join(session_id.to_string())
            .join("stdout.log"),
    )
    .unwrap()
    .len();

    let manifest = fs::read_to_string(
        state
            .path()
            .join("process-sessions")
            .join(session_id.to_string())
            .join("manifest.json"),
    )
    .unwrap();
    assert_eq!(
        output.state,
        ProcessSessionState::Terminated,
        "supervised PTY did not converge after its output limit: {manifest}"
    );
    assert_eq!(
        output.termination_reason,
        Some(ProcessSessionTerminationReason::OutputLimit)
    );
    assert_eq!(
        stdout_len, output_limit,
        "the PTY log did not stop at the durable byte budget"
    );
}

#[tokio::test]
async fn child_inherits_cpu_and_platform_memory_limits() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = limit_reporting_script(trusted.path());
    let governance = ProcessSessionGovernance {
        max_cpu_seconds: 2,
        max_memory_bytes: if cfg!(target_os = "macos") {
            None
        } else {
            Some(256 * 1024 * 1024)
        },
        ..ProcessSessionGovernance::default()
    };
    let manager = PersistentProcessSessionManager::new_with_governance(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let session_id = Uuid::now_v7();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("resource-start"),
            context: context(tenant_id, workspace_root),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let output = loop {
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
        let memory_reported = if cfg!(target_os = "macos") {
            output.stdout.contains("memory-kib:unlimited\n")
        } else {
            output.stdout.contains("memory-kib:262144\n")
        };
        if (output.stdout.contains("cpu:2\n") && memory_reported) || Instant::now() >= deadline {
            break output;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    close_if_running(&manager, &access, session_id).await;

    assert!(output.stdout.contains("cpu:2\n"), "{}", output.stdout);
    let expected_memory = if cfg!(target_os = "macos") {
        "memory-kib:unlimited\n"
    } else {
        "memory-kib:262144\n"
    };
    assert!(output.stdout.contains(expected_memory), "{}", output.stdout);
}

#[derive(Serialize)]
struct LegacyProcessSessionManifest {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Serialize)]
struct PersistedLegacyProcessSessionManifest {
    manifest: LegacyProcessSessionManifest,
    digest: String,
}

#[derive(Serialize)]
struct SchemaTwoProcessSessionManifest {
    schema_version: u32,
    session_id: Uuid,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    source_run_id: Uuid,
    source_attempt_id: Uuid,
    source_tool_call_id: String,
    source_binding_digest: String,
    implementation_digest: String,
    governance_digest: String,
    state: ProcessSessionState,
    pid: Option<u32>,
    process_group_id: Option<i32>,
    exit_code: Option<i32>,
    operation_sequence: u64,
    last_operation: String,
    last_input_digest: Option<String>,
    recovery_count: u32,
    started_at: chrono::DateTime<Utc>,
    execution_deadline_at: chrono::DateTime<Utc>,
    idle_timeout_millis: u64,
    last_activity_at: chrono::DateTime<Utc>,
    max_output_bytes_per_stream: u64,
    max_cpu_seconds: u64,
    max_memory_bytes: Option<u64>,
    observed_stdout_bytes: u64,
    observed_stderr_bytes: u64,
    termination_reason: Option<ProcessSessionTerminationReason>,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Serialize)]
struct PersistedSchemaTwoProcessSessionManifest {
    manifest: SchemaTwoProcessSessionManifest,
    digest: String,
}

#[tokio::test]
async fn schema_two_terminal_history_remains_readable_after_resource_identity_upgrade() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let native = executor(trusted.path(), &executable);
    let implementation_digest = native.implementation_digest().to_owned();
    let manager =
        PersistentProcessSessionManager::new(state.path().to_path_buf(), native, 16 * 1024)
            .unwrap();
    let session_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let now = Utc::now();
    let legacy = SchemaTwoProcessSessionManifest {
        schema_version: 2,
        session_id,
        tenant_id,
        workspace_root: workspace_root.clone(),
        source_run_id: Uuid::now_v7(),
        source_attempt_id: Uuid::now_v7(),
        source_tool_call_id: "schema-two-terminal".into(),
        source_binding_digest: "d".repeat(64),
        implementation_digest,
        governance_digest: "a".repeat(64),
        state: ProcessSessionState::Exited,
        pid: None,
        process_group_id: None,
        exit_code: Some(0),
        operation_sequence: 2,
        last_operation: "exited".into(),
        last_input_digest: None,
        recovery_count: 0,
        started_at: now,
        execution_deadline_at: now + chrono::Duration::hours(1),
        idle_timeout_millis: 60_000,
        last_activity_at: now,
        max_output_bytes_per_stream: 65_536,
        max_cpu_seconds: 60,
        max_memory_bytes: None,
        observed_stdout_bytes: 16,
        observed_stderr_bytes: 0,
        termination_reason: None,
        updated_at: now,
    };
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&legacy).unwrap()));
    let session_dir = state
        .path()
        .join("process-sessions")
        .join(session_id.to_string());
    fs::create_dir(&session_dir).unwrap();
    fs::write(session_dir.join("stdout.log"), b"schema two done\n").unwrap();
    fs::write(session_dir.join("stderr.log"), b"").unwrap();
    fs::write(
        session_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&PersistedSchemaTwoProcessSessionManifest {
            manifest: legacy,
            digest,
        })
        .unwrap(),
    )
    .unwrap();

    let output = manager
        .interact(
            &ProcessSessionAccess {
                tenant_id,
                workspace_root,
            },
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Poll,
            },
        )
        .await
        .expect("schema two terminal history became unreadable");

    assert_eq!(output.state, ProcessSessionState::Exited);
    assert_eq!(output.stdout, "schema two done\n");
}

#[tokio::test]
async fn schema_two_active_session_reattaches_and_rewrites_the_manifest() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let native = executor(trusted.path(), &executable);
    let implementation_digest = native.implementation_digest().to_owned();
    let first = PersistentProcessSessionManager::new(state.path().to_path_buf(), native, 16 * 1024)
        .unwrap();
    let session_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let start_context = context(tenant_id, workspace_root.clone());
    let started = first
        .start(ProcessSessionStartRequest {
            session_id,
            request: request("schema-two-active"),
            context: start_context.clone(),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    let original_pid = started.pid.expect("active session pid");
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = first
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
        if output.stdout.contains("ready\n") {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "session never became ready"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let now = Utc::now();
    let legacy = SchemaTwoProcessSessionManifest {
        schema_version: 2,
        session_id,
        tenant_id,
        workspace_root: workspace_root.clone(),
        source_run_id: start_context.run_id,
        source_attempt_id: start_context.attempt_id,
        source_tool_call_id: "schema-two-active".into(),
        source_binding_digest: "d".repeat(64),
        implementation_digest,
        governance_digest: first.governance_digest(),
        state: ProcessSessionState::Running,
        pid: Some(original_pid),
        process_group_id: i32::try_from(original_pid).ok(),
        exit_code: None,
        operation_sequence: 2,
        last_operation: "started".into(),
        last_input_digest: None,
        recovery_count: 0,
        started_at: now,
        execution_deadline_at: now + chrono::Duration::hours(1),
        idle_timeout_millis: 60_000,
        last_activity_at: now,
        max_output_bytes_per_stream: 64 * 1024 * 1024,
        max_cpu_seconds: 60,
        max_memory_bytes: None,
        observed_stdout_bytes: "ready\n".len() as u64,
        observed_stderr_bytes: 0,
        termination_reason: None,
        updated_at: now,
    };
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&legacy).unwrap()));
    let manifest_path = state
        .path()
        .join("process-sessions")
        .join(session_id.to_string())
        .join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&PersistedSchemaTwoProcessSessionManifest {
            manifest: legacy,
            digest,
        })
        .unwrap(),
    )
    .unwrap();
    let written_legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(written_legacy["manifest"]["schema_version"], 2);

    drop(first);
    let replacement = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    assert_eq!(
        replacement.recover(&access, session_id).await.unwrap(),
        ProcessSessionRecovery::Reattached
    );
    let migrated: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(migrated["manifest"]["schema_version"], 7);
    assert_eq!(migrated["manifest"]["resource_phase"], "active");
    assert_eq!(
        migrated["manifest"]["resource_identity"],
        json!({ "kind": "unix_rlimit" })
    );
    assert_eq!(migrated["manifest"]["recovery_count"], 1);

    close_if_running(&replacement, &access, session_id).await;
}

#[tokio::test]
async fn schema_one_terminal_history_remains_readable_after_governance_upgrade() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let native = executor(trusted.path(), &executable);
    let implementation_digest = native.implementation_digest().to_owned();
    let manager =
        PersistentProcessSessionManager::new(state.path().to_path_buf(), native, 16 * 1024)
            .unwrap();
    let session_id = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let now = Utc::now();
    let legacy = LegacyProcessSessionManifest {
        schema_version: 1,
        session_id,
        tenant_id,
        workspace_root: workspace_root.clone(),
        source_run_id: Uuid::now_v7(),
        source_attempt_id: Uuid::now_v7(),
        source_tool_call_id: "legacy-terminal".into(),
        source_binding_digest: "d".repeat(64),
        implementation_digest,
        state: ProcessSessionState::Exited,
        pid: None,
        process_group_id: None,
        exit_code: Some(0),
        operation_sequence: 2,
        last_operation: "exited".into(),
        last_input_digest: None,
        recovery_count: 0,
        started_at: now,
        updated_at: now,
    };
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&legacy).unwrap()));
    let session_dir = state
        .path()
        .join("process-sessions")
        .join(session_id.to_string());
    fs::create_dir(&session_dir).unwrap();
    fs::write(session_dir.join("stdout.log"), b"legacy complete\n").unwrap();
    fs::write(session_dir.join("stderr.log"), b"").unwrap();
    fs::write(
        session_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&PersistedLegacyProcessSessionManifest {
            manifest: legacy,
            digest,
        })
        .unwrap(),
    )
    .unwrap();

    let output = manager
        .interact(
            &ProcessSessionAccess {
                tenant_id,
                workspace_root,
            },
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Poll,
            },
        )
        .await
        .unwrap();

    assert_eq!(output.state, ProcessSessionState::Exited);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "legacy complete\n");
    assert_eq!(output.termination_reason, None);
}
