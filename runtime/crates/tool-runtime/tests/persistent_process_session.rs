use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PROCESS_CLOSE_TOOL, PROCESS_POLL_TOOL, PROCESS_START_TOOL, PROCESS_WAIT_TOOL,
    PROCESS_WRITE_TOOL, PersistentProcessSessionManager, ProcessSessionAccess,
    ProcessSessionAction, ProcessSessionError, ProcessSessionGovernance, ProcessSessionInteraction,
    ProcessSessionPtySupervisorConfig, ProcessSessionRecovery, ProcessSessionStartRequest,
    ProcessSessionState, ProcessSessionToolExecutor, ProcessSessionToolOperation,
    ToolExecutionContext, ToolExecutionError, ToolExecutor, TrustedNativeExecutor,
    TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("line-session");
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

fn delayed_output_script(root: &Path) -> PathBuf {
    let executable = root.join("delayed-output-session");
    fs::write(
        &executable,
        "#!/bin/sh\nset -eu\n/bin/sleep 1\nprintf 'delayed-ready\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn delayed_echo_script(root: &Path) -> PathBuf {
    let executable = root.join("delayed-echo-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         printf 'ready\\n'\n\
         while IFS= read -r line; do\n\
           /bin/sleep 1\n\
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

fn terminal_probe_script(root: &Path) -> PathBuf {
    let executable = root.join("terminal-probe-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         if [ -t 0 ] && [ -t 1 ]; then\n\
           printf 'terminal=yes\\n'\n\
         else\n\
           printf 'terminal=no\\n'\n\
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

#[cfg(unix)]
fn unavailable_supervisor_script(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = root.join("unavailable-pty-supervisor");
    fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn process_tree_script(root: &Path) -> PathBuf {
    let executable = root.join("process-tree-session");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         trap '' TERM\n\
         (trap '' TERM; while :; do /bin/sleep 1; done) &\n\
         printf '%s\\n' \"$!\" > grandchild.pid\n\
         printf 'ready\\n'\n\
         while :; do /bin/sleep 1; done\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn exiting_leader_with_stubborn_child_script(root: &Path) -> PathBuf {
    let executable = root.join("exiting-session-leader");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         (trap '' TERM; while :; do /bin/sleep 1; done) &\n\
         printf '%s\\n' \"$!\" > stubborn-child.pid\n\
         printf 'leader-done\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 does not mutate the process and the pid came from the
    // Tool itself.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(unix)]
struct TestProcessGroupCleanup {
    process_group_id: Option<i32>,
}

#[cfg(unix)]
impl TestProcessGroupCleanup {
    fn new(pid: u64) -> Self {
        Self {
            process_group_id: i32::try_from(pid).ok(),
        }
    }

    fn disarm(&mut self) {
        self.process_group_id = None;
    }
}

#[cfg(unix)]
impl Drop for TestProcessGroupCleanup {
    fn drop(&mut self) {
        if let Some(process_group_id) = self.process_group_id {
            // SAFETY: the production start path creates a fresh process group
            // whose ID equals the child PID returned by the Tool. This guard is
            // test-only insurance for panic paths before process.close.
            unsafe {
                libc::kill(-process_group_id, libc::SIGKILL);
            }
        }
    }
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

#[cfg(unix)]
fn supervised_manager(
    state_root: PathBuf,
    executor: TrustedNativeExecutor,
) -> PersistentProcessSessionManager {
    PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state_root,
        executor,
        16 * 1024,
        ProcessSessionGovernance::default(),
        Some(ProcessSessionPtySupervisorConfig {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_agent-pty-session-supervisor")),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(10),
        }),
    )
    .unwrap()
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_process_start".into(),
            name: "process.start".into(),
            arguments: json!({}),
        },
        effect: ToolEffect::NonIdempotent,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "d".repeat(64),
    }
}

fn operation_request(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
    effect: ToolEffect,
) -> ToolExecutionRequest {
    let binding_digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&json!({
            "id": id,
            "name": name,
            "arguments": &arguments,
            "effect": effect,
        }))
        .unwrap(),
    ));
    ToolExecutionRequest {
        call: ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        },
        effect,
        sandbox: SandboxClass::TrustedNative,
        binding_digest,
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

async fn poll_until(
    manager: &PersistentProcessSessionManager,
    access: &ProcessSessionAccess,
    session_id: Uuid,
    stdout_cursor: u64,
    expected: &str,
) -> agent_tool_runtime::ProcessSessionOutput {
    let mut cursor = stdout_cursor;
    let mut last = None;
    for _ in 0..500 {
        let output = manager
            .interact(
                access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: cursor,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap();
        if output.stdout.contains(expected) {
            return output;
        }
        if matches!(
            output.state,
            ProcessSessionState::Exited
                | ProcessSessionState::Terminated
                | ProcessSessionState::Indeterminate
        ) {
            panic!(
                "process reached {:?} before output {expected:?}; stdout={:?}; stderr={:?}",
                output.state, output.stdout, output.stderr
            );
        }
        cursor = output.stdout_cursor;
        last = Some(output);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process output never contained {expected:?}; last={last:?}");
}

async fn returned_or_poll_until(
    manager: &PersistentProcessSessionManager,
    access: &ProcessSessionAccess,
    session_id: Uuid,
    returned: agent_tool_runtime::ProcessSessionOutput,
    expected: &str,
) -> agent_tool_runtime::ProcessSessionOutput {
    if returned.stdout.contains(expected) {
        returned
    } else {
        poll_until(
            manager,
            access,
            session_id,
            returned.stdout_cursor,
            expected,
        )
        .await
    }
}

/// The production break this catches is silently restoring the old
/// Runtime-Host-owned PTY path when no external supervisor is configured. A
/// resumable terminal must fail before it creates durable state or spawns a
/// child unless the single PTY owner is available.
#[cfg(unix)]
#[tokio::test]
async fn process_start_tty_requires_an_external_supervisor_before_spawning() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = terminal_probe_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();

    let result = manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, workspace_root.clone()),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await;

    if result.is_ok() {
        let _ = manager
            .interact(
                &ProcessSessionAccess {
                    tenant_id,
                    workspace_root,
                },
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: 0,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Close,
                },
            )
            .await;
    }
    assert!(matches!(
        result,
        Err(ProcessSessionError::InvalidConfiguration(_))
    ));
    assert!(
        fs::read_dir(state.path().join("process-sessions"))
            .unwrap()
            .next()
            .is_none(),
        "a rejected PTY start must not create a session directory"
    );
}

/// The production break this catches is converting a proven pre-spawn PTY
/// supervisor startup failure into an ambiguous side effect and leaving an
/// unrecoverable `starting/unprepared` manifest behind.
#[cfg(unix)]
#[tokio::test]
async fn unavailable_pty_supervisor_fails_before_spawn_and_closes_the_start_intent() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = terminal_probe_script(trusted.path());
    let unavailable_supervisor = unavailable_supervisor_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        ProcessSessionGovernance::default(),
        Some(ProcessSessionPtySupervisorConfig {
            executable: unavailable_supervisor,
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_millis(100),
        }),
    )
    .unwrap();

    let result = manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, workspace_root),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await;

    assert!(matches!(result, Err(ProcessSessionError::Io(_))));
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            state
                .path()
                .join("process-sessions")
                .join(session_id.to_string())
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["manifest"]["state"], "terminated");
    assert_eq!(manifest["manifest"]["resource_phase"], "cleaned");
    assert_eq!(manifest["manifest"]["last_operation"], "start_failed");
    assert_eq!(manifest["manifest"]["termination_reason"], "start_failed");
}

/// The production break this catches is treating an interactive process as
/// three ordinary pipes: terminal-aware programs would disable prompts,
/// screen control and line editing even when the model explicitly requests a
/// TTY.
#[cfg(unix)]
#[tokio::test]
async fn process_start_tty_allocates_a_real_terminal() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = terminal_probe_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = std::sync::Arc::new(supervised_manager(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
    ));
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let started = start
        .execute(
            operation_request(
                "call_terminal_start",
                PROCESS_START_TOOL,
                json!({"tty": true, "cols": 80, "rows": 24}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .expect("an explicitly requested PTY should start");
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root,
    };
    let output = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "terminal=yes",
    )
    .await;
    assert!(
        output.stdout.contains("terminal=yes"),
        "the child must observe a real PTY, not terminal-like environment variables"
    );
    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: output.stdout_cursor,
                stderr_cursor: output.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
}

/// The production break this catches is leaving process.write connected to
/// the pipe-mode FIFO after the child moved to a PTY: the terminal would start
/// successfully but could never receive interactive model input.
#[cfg(unix)]
#[tokio::test]
async fn process_write_sends_input_to_the_live_terminal() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = terminal_probe_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = std::sync::Arc::new(supervised_manager(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
    ));
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let started = start
        .execute(
            operation_request(
                "call_terminal_write_start",
                PROCESS_START_TOOL,
                json!({"tty": true}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    #[cfg(unix)]
    let mut process_cleanup =
        TestProcessGroupCleanup::new(started.content["pid"].as_u64().unwrap());
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root,
    };
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "terminal=yes",
    )
    .await;
    let written = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Write {
                    bytes: b"from-model\n".to_vec(),
                },
            },
        )
        .await
        .expect("process.write should target the live PTY master");
    let echoed = returned_or_poll_until(
        manager.as_ref(),
        &access,
        session_id,
        written,
        "got:from-model",
    )
    .await;
    assert!(echoed.stdout.contains("got:from-model"));
    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: echoed.stdout_cursor,
                stderr_cursor: echoed.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    #[cfg(unix)]
    process_cleanup.disarm();
}

/// The production break this catches is persisting only a PID/process group:
/// a replacement Host would have no durable resource-backend identity to
/// validate before reattaching or terminating the process tree.
#[tokio::test]
async fn manifest_persists_the_resource_backend_identity_used_for_recovery() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();

    let manifest_path = state
        .path()
        .join("process-sessions")
        .join(session_id.to_string())
        .join("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manager
        .interact(
            &ProcessSessionAccess {
                tenant_id,
                workspace_root,
            },
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    let terminal_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();

    assert_eq!(manifest["manifest"]["schema_version"], 7);
    assert_eq!(
        manifest["manifest"]["resource_identity"],
        json!({ "kind": "unix_rlimit" })
    );
    assert_eq!(manifest["manifest"]["observed_cpu_usage_micros"], 0);
    assert_eq!(manifest["manifest"]["resource_phase"], "active");
    assert_eq!(terminal_manifest["manifest"]["resource_phase"], "cleaned");
}

/// The production break this catches is storing the session only in one Host's
/// memory: dropping that owner would make a still-running child either
/// unreachable or silently restartable.
#[tokio::test]
async fn replacement_manager_reattaches_writes_reads_and_closes_the_original_process() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let first = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    let started = first
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, access.workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(started.session_id, session_id);
    assert_eq!(started.state, ProcessSessionState::Running);
    let ready = poll_until(&first, &access, session_id, 0, "ready\n").await;
    let original_pid = ready.pid.expect("running process pid");

    drop(first);
    let replacement = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    let recovery = replacement.recover(&access, session_id).await.unwrap();
    assert!(matches!(recovery, ProcessSessionRecovery::Reattached));

    let written = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Write {
                    bytes: b"hello replacement\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(written.pid, Some(original_pid));
    let echoed = returned_or_poll_until(
        &replacement,
        &access,
        session_id,
        written,
        "got:hello replacement\n",
    )
    .await;
    assert_eq!(echoed.pid, Some(original_pid));

    let closed = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: echoed.stdout_cursor,
                stderr_cursor: echoed.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        closed.state,
        ProcessSessionState::Exited | ProcessSessionState::Terminated
    ));
    assert_eq!(closed.pid, None);
    assert!(matches!(
        replacement.recover(&access, session_id).await.unwrap(),
        ProcessSessionRecovery::Terminated
    ));
}

#[tokio::test]
async fn tenant_workspace_manifest_and_cursor_boundaries_fail_closed() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other_workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, access.workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    let ready = poll_until(&manager, &access, session_id, 0, "ready\n").await;

    for denied in [
        ProcessSessionAccess {
            tenant_id: Uuid::now_v7(),
            workspace_root: access.workspace_root.clone(),
        },
        ProcessSessionAccess {
            tenant_id,
            workspace_root: other_workspace.path().canonicalize().unwrap(),
        },
    ] {
        let error = manager
            .interact(
                &denied,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: ready.stdout_cursor,
                    stderr_cursor: ready.stderr_cursor,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, ProcessSessionError::AccessDenied);
    }
    let invalid_cursor = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: u64::MAX,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Poll,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(invalid_cursor, ProcessSessionError::InvalidCursor);

    let manifest_path = state
        .path()
        .join("process-sessions")
        .join(session_id.to_string())
        .join("manifest.json");
    let original_manifest = fs::read(&manifest_path).unwrap();
    let mut tampered = String::from_utf8(original_manifest.clone()).unwrap();
    assert!(tampered.contains("\"last_operation\": \"started\""));
    tampered = tampered.replace(
        "\"last_operation\": \"started\"",
        "\"last_operation\": \"tampered\"",
    );
    fs::write(&manifest_path, tampered).unwrap();
    let error = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Poll,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error, ProcessSessionError::Indeterminate);
    fs::write(&manifest_path, original_manifest).unwrap();

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn close_escalates_and_reaps_the_entire_registered_process_group() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = process_tree_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, access.workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    let ready = poll_until(&manager, &access, session_id, 0, "ready\n").await;
    let marker = workspace.path().join("grandchild.pid");
    for _ in 0..100 {
        if marker.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let grandchild_pid = fs::read_to_string(&marker)
        .expect("the Tool never recorded its grandchild")
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(process_alive(grandchild_pid));

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    for _ in 0..100 {
        if !process_alive(grandchild_pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("grandchild {grandchild_pid} survived process-session close");
}

#[cfg(unix)]
#[tokio::test]
async fn natural_leader_exit_also_reaps_a_stubborn_background_descendant() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = exiting_leader_with_stubborn_child_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, access.workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    let marker = workspace.path().join("stubborn-child.pid");
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
            ProcessSessionState::Exited
                | ProcessSessionState::Terminated
                | ProcessSessionState::Indeterminate
        );
        if marker.is_file() && terminal {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(marker.is_file(), "background descendant never started");
    assert!(terminal, "session leader never reached a terminal state");
    let child_pid = fs::read_to_string(marker)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    for _ in 0..100 {
        if !process_alive(child_pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("background descendant {child_pid} survived natural leader exit");
}

#[cfg(unix)]
#[tokio::test]
async fn interrupt_reaches_the_registered_process_group_and_converges_terminal() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let manager = PersistentProcessSessionManager::new(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
    )
    .unwrap();
    manager
        .start(ProcessSessionStartRequest {
            session_id,
            request: request(),
            context: context(tenant_id, access.workspace_root.clone()),
            initial_stdin: Vec::new(),
        })
        .await
        .unwrap();
    let ready = poll_until(&manager, &access, session_id, 0, "ready\n").await;
    let interrupted = manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Interrupt,
            },
        )
        .await
        .unwrap();
    assert_ne!(
        interrupted.state,
        ProcessSessionState::Indeterminate,
        "the registered identity vanished before SIGINT could be delivered"
    );
    let mut last = None;
    for _ in 0..200 {
        let output = manager
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: ready.stdout_cursor,
                    stderr_cursor: ready.stderr_cursor,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap();
        if matches!(
            output.state,
            ProcessSessionState::Exited | ProcessSessionState::Terminated
        ) {
            assert_eq!(output.pid, None);
            return;
        }
        last = Some(output);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Which of two different problems this is turns on one fact, and the
    // message used to carry neither: if the leader is gone, the process died
    // and nothing published it -- the reaper task that owns `child.wait()` is
    // the only thing that ever does, and `interact(Poll)` does not check
    // liveness, so one missed wake-up is a permanent wrong answer rather than
    // a late one. If the leader is alive, SIGINT never reached it and the
    // question is about signal delivery instead. Measured in isolation this
    // converges in 12-24ms, so a failure here is a stall and not a tight
    // bound; see docs/evidence/2026-08-19-process-session-tests-under-load.md.
    let leader = last.as_ref().and_then(|output| output.pid);
    panic!(
        "SIGINT never reached a terminal process-session state; \
         leader={leader:?} leader_alive={:?} last={last:?}",
        leader.map(process_alive),
    );
}

#[tokio::test]
async fn model_visible_process_tools_share_one_durable_session_protocol() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let write =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Write);
    let poll = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Poll);
    let close =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Close);
    let workspace_root = workspace.path().canonicalize().unwrap();

    let started = start
        .execute(
            operation_request(
                "call_start",
                PROCESS_START_TOOL,
                json!({"initial_stdin": ""}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    let session_id = started.content["session_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let mut stdout_cursor = started.content["stdout_cursor"].as_u64().unwrap();
    let mut stderr_cursor = started.content["stderr_cursor"].as_u64().unwrap();

    for _ in 0..200 {
        let output = poll
            .execute(
                operation_request(
                    "call_poll_ready",
                    PROCESS_POLL_TOOL,
                    json!({
                        "session_id": session_id,
                        "stdout_cursor": stdout_cursor,
                        "stderr_cursor": stderr_cursor
                    }),
                    ToolEffect::Pure,
                ),
                context(tenant_id, workspace_root.clone()),
            )
            .await
            .unwrap();
        stdout_cursor = output.content["stdout_cursor"].as_u64().unwrap();
        stderr_cursor = output.content["stderr_cursor"].as_u64().unwrap();
        if output.content["stdout"]
            .as_str()
            .unwrap()
            .contains("ready\n")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let written = write
        .execute(
            operation_request(
                "call_write",
                PROCESS_WRITE_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": stdout_cursor,
                    "stderr_cursor": stderr_cursor,
                    "stdin": "model-visible\n"
                }),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    stdout_cursor = written.content["stdout_cursor"].as_u64().unwrap();
    stderr_cursor = written.content["stderr_cursor"].as_u64().unwrap();
    let mut echo_seen = written.content["stdout"]
        .as_str()
        .unwrap()
        .contains("got:model-visible\n");
    for _ in 0..200 {
        if echo_seen {
            break;
        }
        let output = poll
            .execute(
                operation_request(
                    "call_poll_echo",
                    PROCESS_POLL_TOOL,
                    json!({
                        "session_id": session_id,
                        "stdout_cursor": stdout_cursor,
                        "stderr_cursor": stderr_cursor
                    }),
                    ToolEffect::Pure,
                ),
                context(tenant_id, workspace_root.clone()),
            )
            .await
            .unwrap();
        stdout_cursor = output.content["stdout_cursor"].as_u64().unwrap();
        stderr_cursor = output.content["stderr_cursor"].as_u64().unwrap();
        echo_seen = output.content["stdout"]
            .as_str()
            .unwrap()
            .contains("got:model-visible\n");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        echo_seen,
        "process Tool output never reached the model protocol"
    );

    let closed = close
        .execute(
            operation_request(
                "call_close",
                PROCESS_CLOSE_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": stdout_cursor,
                    "stderr_cursor": stderr_cursor
                }),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root),
        )
        .await
        .unwrap();
    assert!(matches!(
        closed.content["state"].as_str(),
        Some("exited" | "terminated")
    ));
}

/// The production break this catches is returning from process.start before
/// its explicitly bounded first-output window. That would force the model to
/// spend another Tool call polling a child that is already making progress.
#[tokio::test]
async fn process_start_can_yield_until_first_output_in_the_same_tool_call() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = delayed_output_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start = ProcessSessionToolExecutor::new(manager, ProcessSessionToolOperation::Start);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let mut start_context = context(tenant_id, workspace_root);
    start_context.timeout = Duration::from_secs(15);

    let started = start
        .execute(
            operation_request(
                "call_start_with_yield",
                PROCESS_START_TOOL,
                json!({"yield_time_ms": 10_000}),
                ToolEffect::NonIdempotent,
            ),
            start_context,
        )
        .await
        .expect("process.start should wait for the first durable output when yield is requested");

    assert_eq!(started.content["stdout"], "delayed-ready\n");
    assert!(matches!(
        started.content["state"].as_str(),
        Some("running" | "exited")
    ));
}

/// The production break this catches is acknowledging a process write before
/// its bounded response window. A model should not need a second poll Tool call
/// just because an interactive child responds shortly after accepting input.
#[tokio::test]
async fn process_write_can_yield_until_response_in_the_same_tool_call() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = delayed_echo_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let write =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Write);
    let close =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Close);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let started = start
        .execute(
            operation_request(
                "call_write_yield_start",
                PROCESS_START_TOOL,
                json!({}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "ready\n",
    )
    .await;

    let written = write
        .execute(
            operation_request(
                "call_write_with_yield",
                PROCESS_WRITE_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": ready.stdout_cursor,
                    "stderr_cursor": ready.stderr_cursor,
                    "stdin": "model-visible\n",
                    "yield_time_ms": 4_000
                }),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .expect("process.write should wait for the bounded child response");

    assert_eq!(written.content["stdout"], "got:model-visible\n");

    close
        .execute(
            operation_request(
                "call_write_yield_close",
                PROCESS_CLOSE_TOOL,
                json!({
                    "session_id": written.content["session_id"],
                    "stdout_cursor": written.content["stdout_cursor"],
                    "stderr_cursor": written.content["stderr_cursor"]
                }),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root),
        )
        .await
        .unwrap();
}

/// The production break this catches is implementing wait as an immediate
/// poll: a delayed child would force another model round instead of yielding
/// the same Tool call until output or terminal state is durable.
#[tokio::test]
async fn process_wait_yields_until_delayed_output_is_durable() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = delayed_output_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let started = start
        .execute(
            operation_request(
                "call_delayed_start",
                PROCESS_START_TOOL,
                json!({}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    assert!(started.content["stdout"].as_str().unwrap().is_empty());

    let waited = wait
        .execute(
            operation_request(
                "call_delayed_wait",
                PROCESS_WAIT_TOOL,
                json!({
                    "session_id": started.content["session_id"],
                    "stdout_cursor": started.content["stdout_cursor"],
                    "stderr_cursor": started.content["stderr_cursor"],
                    "yield_time_ms": 5_000
                }),
                ToolEffect::Pure,
            ),
            context(tenant_id, workspace_root),
        )
        .await
        .unwrap();

    assert!(matches!(
        waited.content["state"].as_str(),
        Some("running" | "exited")
    ));
    assert_eq!(waited.content["stdout"], "delayed-ready\n");

    let session_id = Uuid::parse_str(waited.content["session_id"].as_str().unwrap()).unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let mut terminal = false;
    for _ in 0..100 {
        let output = manager
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor: waited.content["stdout_cursor"].as_u64().unwrap(),
                    stderr_cursor: waited.content["stderr_cursor"].as_u64().unwrap(),
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
        ) {
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        terminal,
        "the output-producing process never converged terminal"
    );
}

/// The production break this catches is multiplying one durable filesystem
/// observation loop by every waiting Run. A single live process may be
/// observed by many Runs, but their long-polls must share one session observer
/// and all wake from the same durable output transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_coalesces_one_thousand_process_waiters() {
    const WAITER_COUNT: usize = 1_000;

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let started = start
        .execute(
            operation_request(
                "call_fanout_start",
                PROCESS_START_TOOL,
                json!({}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    #[cfg(unix)]
    let mut process_cleanup =
        TestProcessGroupCleanup::new(started.content["pid"].as_u64().unwrap());
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "ready\n",
    )
    .await;

    let mut waiters = tokio::task::JoinSet::new();
    for ordinal in 0..WAITER_COUNT {
        let wait = wait.clone();
        let workspace_root = workspace_root.clone();
        waiters.spawn(async move {
            wait.execute(
                operation_request(
                    &format!("call_fanout_wait_{ordinal}"),
                    PROCESS_WAIT_TOOL,
                    json!({
                        "session_id": session_id,
                        "stdout_cursor": ready.stdout_cursor,
                        "stderr_cursor": ready.stderr_cursor,
                        "yield_time_ms": 4_000
                    }),
                    ToolEffect::Pure,
                ),
                context(tenant_id, workspace_root),
            )
            .await
        });
    }

    let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = manager.wait_observation_snapshot();
        if snapshot.active_waiters == WAITER_COUNT {
            break;
        }
        assert!(
            tokio::time::Instant::now() < registration_deadline,
            "only {} of {WAITER_COUNT} waiters registered",
            snapshot.active_waiters
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let observations_before = manager.wait_observation_snapshot().filesystem_observations;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let waiting_snapshot = manager.wait_observation_snapshot();
    let observation_delta = waiting_snapshot
        .filesystem_observations
        .saturating_sub(observations_before);
    assert_eq!(waiting_snapshot.active_observers, 1);
    assert!(
        observation_delta <= 10,
        "one session performed {observation_delta} filesystem observations for {WAITER_COUNT} waiters"
    );

    let wake_started_at = tokio::time::Instant::now();
    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Write {
                    bytes: b"fanout\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();

    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut completed = 0;
        while let Some(result) = waiters.join_next().await {
            let output = result.unwrap().unwrap();
            assert!(
                output.content["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("got:fanout\n")
            );
            completed += 1;
        }
        completed
    })
    .await
    .expect("the shared durable output transition did not wake every waiter");
    assert_eq!(completed, WAITER_COUNT);
    assert!(
        wake_started_at.elapsed() < Duration::from_secs(2),
        "waking {WAITER_COUNT} waiters took {:?}; snapshot={:?}",
        wake_started_at.elapsed(),
        manager.wait_observation_snapshot()
    );
    assert_eq!(manager.wait_observation_snapshot().active_waiters, 0);

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    #[cfg(unix)]
    process_cleanup.disarm();
}

/// The production break this catches is retaining the shared background
/// observer after the last waiting Run is cancelled. Long-lived sessions must
/// not accumulate idle observation tasks.
#[tokio::test]
async fn cancelling_the_last_process_waiter_retires_its_shared_observer() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let started = start
        .execute(
            operation_request(
                "call_cancel_wait_start",
                PROCESS_START_TOOL,
                json!({}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    #[cfg(unix)]
    let mut process_cleanup =
        TestProcessGroupCleanup::new(started.content["pid"].as_u64().unwrap());
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "ready\n",
    )
    .await;

    let cancellation = CancellationToken::new();
    let mut wait_context = context(tenant_id, workspace_root);
    wait_context.cancellation = cancellation.clone();
    let waiting = tokio::spawn(async move {
        wait.execute(
            operation_request(
                "call_cancel_wait",
                PROCESS_WAIT_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": ready.stdout_cursor,
                    "stderr_cursor": ready.stderr_cursor,
                    "yield_time_ms": 4_000
                }),
                ToolEffect::Pure,
            ),
            wait_context,
        )
        .await
    });

    let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = manager.wait_observation_snapshot();
        if snapshot.active_waiters == 1 && snapshot.active_observers == 1 {
            break;
        }
        assert!(tokio::time::Instant::now() < registration_deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("cancelled process.wait did not stop")
        .unwrap()
        .expect_err("cancelled process.wait unexpectedly succeeded");
    assert!(matches!(error, ToolExecutionError::Cancelled));

    let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let snapshot = manager.wait_observation_snapshot();
        if snapshot.active_waiters == 0 && snapshot.active_observers == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < cleanup_deadline,
            "cancelled wait leaked observation state: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: 0,
                stderr_cursor: 0,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    #[cfg(unix)]
    process_cleanup.disarm();
}

/// The production break this catches is observing only child-owned pipe logs.
/// Resumable PTY output is appended by an external supervisor and must wake the
/// same durable wait protocol after the Runtime Host has lost process ownership.
#[cfg(unix)]
#[tokio::test]
async fn process_wait_observes_external_pty_supervisor_output() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(supervised_manager(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
    ));
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let started = start
        .execute(
            operation_request(
                "call_pty_wait_start",
                PROCESS_START_TOOL,
                json!({"tty": true, "cols": 100, "rows": 32}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    let mut process_cleanup =
        TestProcessGroupCleanup::new(started.content["pid"].as_u64().unwrap());
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "ready",
    )
    .await;

    let waiting = tokio::spawn(async move {
        wait.execute(
            operation_request(
                "call_pty_wait",
                PROCESS_WAIT_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": ready.stdout_cursor,
                    "stderr_cursor": ready.stderr_cursor,
                    "yield_time_ms": 4_000
                }),
                ToolEffect::Pure,
            ),
            context(tenant_id, workspace_root),
        )
        .await
    });
    let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while manager.wait_observation_snapshot().active_waiters != 1 {
        assert!(tokio::time::Instant::now() < registration_deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Write {
                    bytes: b"pty-event\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("external PTY output did not wake process.wait")
        .unwrap()
        .unwrap();
    assert!(
        output.content["stdout"]
            .as_str()
            .unwrap()
            .contains("got:pty-event")
    );

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: output.content["stdout_cursor"].as_u64().unwrap(),
                stderr_cursor: output.content["stderr_cursor"].as_u64().unwrap(),
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    process_cleanup.disarm();
}

/// The production break this catches is allowing a model-supplied wait to
/// outlive the Run-frozen Tool timeout. That would let a Pure convenience
/// operation bypass the same execution budget enforced for every other Tool.
#[tokio::test]
async fn process_wait_rejects_a_yield_longer_than_the_tool_execution_budget() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let start =
        ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Start);
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let started = start
        .execute(
            operation_request(
                "call_bounded_wait_start",
                PROCESS_START_TOOL,
                json!({}),
                ToolEffect::NonIdempotent,
            ),
            context(tenant_id, workspace_root.clone()),
        )
        .await
        .unwrap();
    let session_id = Uuid::parse_str(started.content["session_id"].as_str().unwrap()).unwrap();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace_root.clone(),
    };
    let ready = poll_until(
        manager.as_ref(),
        &access,
        session_id,
        started.content["stdout_cursor"].as_u64().unwrap(),
        "ready",
    )
    .await;
    let mut bounded_context = context(tenant_id, workspace_root);
    bounded_context.timeout = Duration::from_millis(20);

    let error = wait
        .execute(
            operation_request(
                "call_overlong_wait",
                PROCESS_WAIT_TOOL,
                json!({
                    "session_id": session_id,
                    "stdout_cursor": ready.stdout_cursor,
                    "stderr_cursor": ready.stderr_cursor,
                    "yield_time_ms": 100
                }),
                ToolEffect::Pure,
            ),
            bounded_context,
        )
        .await
        .expect_err("wait must not exceed the frozen Tool timeout");
    assert!(error.to_string().contains("Tool execution timeout"));

    manager
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: ready.stdout_cursor,
                stderr_cursor: ready.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
}
