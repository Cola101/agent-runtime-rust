use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionGovernance, ProcessSessionInteraction, ProcessSessionPtySupervisorConfig,
    ProcessSessionRecovery, ProcessSessionStartRequest, ToolExecutionContext,
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
    let executable = root.join("line-session");
    fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
printf 'ready\n'
while IFS= read -r line; do
  if [ "$line" = size ]; then
    /usr/bin/python3 -c 'import fcntl,struct,termios; print(*struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))[:2])'
  else
    printf 'got:%s\n' "$line"
  fi
done
"#,
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

fn pty_manager(
    state_root: PathBuf,
    trusted_root: &Path,
    executable: &Path,
) -> PersistentProcessSessionManager {
    PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state_root,
        executor(trusted_root, executable),
        16 * 1024,
        ProcessSessionGovernance::default(),
        Some(ProcessSessionPtySupervisorConfig {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_agent-pty-session-supervisor")),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(5),
        }),
    )
    .unwrap()
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "crash_owner_start".into(),
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

async fn poll_for(
    manager: &PersistentProcessSessionManager,
    access: &ProcessSessionAccess,
    session_id: Uuid,
    mut stdout_cursor: u64,
    expected: &str,
) -> agent_tool_runtime::ProcessSessionOutput {
    let mut observed = String::new();
    for _ in 0..500 {
        let output = manager
            .interact(
                access,
                ProcessSessionInteraction {
                    session_id,
                    stdout_cursor,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap();
        observed.push_str(&output.stdout);
        if output.stdout.contains(expected) {
            return output;
        }
        stdout_cursor = output.stdout_cursor;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process output never contained {expected:?}; observed {observed:?}");
}

async fn lifecycle_until(state_root: &Path, expected_state: &str) -> serde_json::Value {
    let path = state_root.join("process-sessions.supervisor-state.json");
    let mut last = None;
    for _ in 0..700 {
        if let Ok(bytes) = fs::read(&path)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            if value["lifecycle"]["state"] == expected_state {
                return value;
            }
            last = Some(value);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("PTY supervisor lifecycle never reached {expected_state:?}; last={last:?}");
}

/// The production break this catches is an idle supervisor disappearing with
/// no durable explanation. A replacement must be able to distinguish a clean
/// idle shutdown from a crash without relying on transient process output.
#[tokio::test]
async fn supervisor_persists_a_clean_idle_shutdown_with_its_capabilities() {
    use std::os::unix::fs::PermissionsExt;

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let manager = pty_manager(state.path().to_path_buf(), trusted.path(), &executable);
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, access.workspace_root.clone()),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await
        .unwrap();
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

    let lifecycle = lifecycle_until(state.path(), "stopped").await;
    assert_eq!(lifecycle["lifecycle"]["shutdown_reason"], "idle_timeout");
    assert_eq!(lifecycle["lifecycle"]["active_sessions"], 0);
    assert_eq!(lifecycle["lifecycle"]["protocol_version"], 3);
    let capabilities = lifecycle["lifecycle"]["capabilities"].as_array().unwrap();
    for required in [
        "pty.start.generation-fenced.v1",
        "pty.status.v1",
        "pty.write.v1",
        "pty.resize.v1",
        "pty.lifecycle.v1",
    ] {
        assert!(capabilities.iter().any(|actual| actual == required));
    }
    assert_eq!(
        fs::metadata(state.path().join("process-sessions.supervisor-state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// Runs alone in this integration binary because it recursively starts that
/// binary as the disposable owner. Other process-session tests still run in
/// parallel and cover concurrent starts; this test isolates only the crash
/// harness from inheriting the parent test harness's transient descriptors.
#[tokio::test(flavor = "current_thread")]
async fn replacement_host_process_reattaches_after_the_owner_process_crashes() {
    const OWNER_MODE: &str = "AGENT_PROCESS_SESSION_CRASH_OWNER";
    const TEST_NAME: &str = "replacement_host_process_reattaches_after_the_owner_process_crashes";

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
        let manager = pty_manager(state_root, &trusted_root, &executable);
        let access = ProcessSessionAccess {
            tenant_id,
            workspace_root: workspace_root.canonicalize().unwrap(),
        };
        manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, access.workspace_root.clone()),
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap();
        let ready = poll_for(&manager, &access, session_id, 0, "ready\n").await;
        fs::write(
            std::env::var_os("AGENT_PROCESS_SESSION_HANDOFF").unwrap(),
            ready.pid.unwrap().to_string(),
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
    let replacement = pty_manager(state.path().to_path_buf(), trusted.path(), &executable);
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    assert_eq!(
        replacement.recover(&access, session_id).await.unwrap(),
        ProcessSessionRecovery::Reattached
    );
    let written = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: "ready\n".len() as u64,
                stderr_cursor: 0,
                action: ProcessSessionAction::Write {
                    bytes: b"after crash\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(written.pid, Some(original_pid));
    let echoed = if written.stdout.contains("got:after crash\n") {
        written
    } else {
        poll_for(
            &replacement,
            &access,
            session_id,
            written.stdout_cursor,
            "got:after crash\n",
        )
        .await
    };
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
        agent_tool_runtime::ProcessSessionState::Exited
            | agent_tool_runtime::ProcessSessionState::Terminated
    ));
}

/// The production break this catches is keeping the PTY master inside the
/// Runtime Host: when that Host exits, a replacement can still see the durable
/// child identity but cannot write to the original terminal.
#[tokio::test(flavor = "current_thread")]
async fn replacement_host_process_writes_to_the_original_pty_after_owner_crash() {
    const OWNER_MODE: &str = "AGENT_PTY_SESSION_CRASH_OWNER";
    const TEST_NAME: &str = "replacement_host_process_writes_to_the_original_pty_after_owner_crash";

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
        let manager = pty_manager(state_root, &trusted_root, &executable);
        let access = ProcessSessionAccess {
            tenant_id,
            workspace_root: workspace_root.canonicalize().unwrap(),
        };
        manager
            .start_pty(
                ProcessSessionStartRequest {
                    session_id,
                    request: request(),
                    context: context(tenant_id, access.workspace_root.clone()),
                    initial_stdin: Vec::new(),
                },
                100,
                31,
            )
            .await
            .unwrap();
        let ready = poll_for(&manager, &access, session_id, 0, "ready").await;
        fs::write(
            std::env::var_os("AGENT_PROCESS_SESSION_HANDOFF").unwrap(),
            ready.pid.unwrap().to_string(),
        )
        .unwrap();
        std::process::exit(74);
    }

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let handoff = state.path().join("pty-owner-handoff");
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
    assert_eq!(status.code(), Some(74));
    let original_pid = fs::read_to_string(&handoff)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let replacement = pty_manager(state.path().to_path_buf(), trusted.path(), &executable);
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };

    assert_eq!(
        replacement.recover(&access, session_id).await.unwrap(),
        ProcessSessionRecovery::Reattached
    );
    let written = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: "ready\r\n".len() as u64,
                stderr_cursor: 0,
                action: ProcessSessionAction::Write {
                    bytes: b"after pty crash\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(written.pid, Some(original_pid));
    let echoed = if written.stdout.contains("got:after pty crash") {
        written
    } else {
        poll_for(
            &replacement,
            &access,
            session_id,
            written.stdout_cursor,
            "got:after pty crash",
        )
        .await
    };
    let resized = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: echoed.stdout_cursor,
                stderr_cursor: echoed.stderr_cursor,
                action: ProcessSessionAction::Resize {
                    cols: 132,
                    rows: 43,
                },
            },
        )
        .await
        .unwrap();
    let resized = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: resized.stdout_cursor,
                stderr_cursor: resized.stderr_cursor,
                action: ProcessSessionAction::Write {
                    bytes: b"size\n".to_vec(),
                },
            },
        )
        .await
        .unwrap();
    let resized = if resized.stdout.contains("43 132") {
        resized
    } else {
        poll_for(
            &replacement,
            &access,
            session_id,
            resized.stdout_cursor,
            "43 132",
        )
        .await
    };
    let closed = replacement
        .interact(
            &access,
            ProcessSessionInteraction {
                session_id,
                stdout_cursor: resized.stdout_cursor,
                stderr_cursor: resized.stderr_cursor,
                action: ProcessSessionAction::Close,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        closed.state,
        agent_tool_runtime::ProcessSessionState::Exited
            | agent_tool_runtime::ProcessSessionState::Terminated
    ));
}

#[tokio::test]
async fn supervised_pty_state_is_private_to_the_runtime_owner() {
    use std::os::unix::fs::PermissionsExt;

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let manager = pty_manager(state.path().to_path_buf(), trusted.path(), &executable);
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, access.workspace_root.clone()),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await
        .unwrap();

    let sessions_root = state.path().join("process-sessions");
    let session_dir = sessions_root.join(session_id.to_string());
    for directory in [&sessions_root, &session_dir] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "{} is readable by another local user",
            directory.display()
        );
    }
    for name in [
        "stdout.log",
        "stderr.log",
        "identity.lock",
        "control.lock",
        "sweep.lock",
        "stdin.fifo",
        "manifest.json",
        "terminal.json",
    ] {
        let path = session_dir.join(name);
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{} is readable by another local user",
            path.display()
        );
    }
    assert_eq!(
        fs::metadata(state.path().join("process-sessions.supervisor.token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

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
}

#[tokio::test]
async fn replacement_fails_closed_when_the_pty_supervisor_is_lost() {
    use std::os::unix::fs::PermissionsExt;

    struct ProcessCleanup {
        supervisor_pid: i32,
        process_group_id: i32,
    }
    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            unsafe {
                libc::kill(self.supervisor_pid, libc::SIGKILL);
                libc::kill(-self.process_group_id, libc::SIGKILL);
            }
        }
    }

    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let supervisor_pid_path = state.path().join("supervisor.pid");
    let supervisor_wrapper = trusted.path().join("pty-supervisor-wrapper");
    fs::write(
        &supervisor_wrapper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec '{}' \"$@\"\n",
            supervisor_pid_path.display(),
            env!("CARGO_BIN_EXE_agent-pty-session-supervisor")
        ),
    )
    .unwrap();
    fs::set_permissions(&supervisor_wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let manager = PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        ProcessSessionGovernance::default(),
        Some(ProcessSessionPtySupervisorConfig {
            executable: supervisor_wrapper.clone(),
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(5),
        }),
    )
    .unwrap();
    let tenant_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let access = ProcessSessionAccess {
        tenant_id,
        workspace_root: workspace.path().canonicalize().unwrap(),
    };
    let started = manager
        .start_pty(
            ProcessSessionStartRequest {
                session_id,
                request: request(),
                context: context(tenant_id, access.workspace_root.clone()),
                initial_stdin: Vec::new(),
            },
            80,
            24,
        )
        .await
        .unwrap();
    let process_group_id = i32::try_from(started.pid.unwrap()).unwrap();
    let original_lifecycle = lifecycle_until(state.path(), "ready").await;
    let original_supervisor_id = original_lifecycle["lifecycle"]["supervisor_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let supervisor_pid = fs::read_to_string(&supervisor_pid_path)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let _cleanup = ProcessCleanup {
        supervisor_pid,
        process_group_id,
    };
    assert_eq!(unsafe { libc::kill(supervisor_pid, libc::SIGKILL) }, 0);
    for _ in 0..100 {
        if unsafe { libc::kill(supervisor_pid, 0) } != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let replacement = PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
        state.path().to_path_buf(),
        executor(trusted.path(), &executable),
        16 * 1024,
        ProcessSessionGovernance::default(),
        Some(ProcessSessionPtySupervisorConfig {
            executable: supervisor_wrapper,
            fixed_args: Vec::new(),
            startup_timeout: Duration::from_secs(5),
        }),
    )
    .unwrap();
    assert_eq!(
        replacement.recover(&access, session_id).await.unwrap(),
        ProcessSessionRecovery::Indeterminate,
        "a replacement must not claim reattachment after terminal control was lost"
    );
    let mut replacement_lifecycle = None;
    for _ in 0..200 {
        let observed = lifecycle_until(state.path(), "ready").await;
        if observed["lifecycle"]["supervisor_id"] != original_supervisor_id {
            replacement_lifecycle = Some(observed);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let replacement_lifecycle =
        replacement_lifecycle.expect("a replacement supervisor lifecycle was not persisted");
    assert_eq!(
        replacement_lifecycle["lifecycle"]["predecessor"]["supervisor_id"],
        original_supervisor_id
    );
    assert_eq!(
        replacement_lifecycle["lifecycle"]["predecessor"]["clean_shutdown"],
        false
    );
    for _ in 0..100 {
        if unsafe { libc::kill(-process_group_id, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the process group survived after its PTY supervisor was lost");
}
