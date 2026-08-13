use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    PROCESS_WAIT_TOOL, PersistentProcessSessionManager, ProcessSessionAccess, ProcessSessionAction,
    ProcessSessionInteraction, ProcessSessionStartRequest, ProcessSessionToolExecutor,
    ProcessSessionToolOperation, ToolExecutionContext, ToolExecutionError, ToolExecutor,
    TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TENANT_COUNT: usize = 8;
const SESSION_COUNT: usize = 64;
const WAITERS_PER_SESSION: usize = 16;
const TOTAL_WAITERS: usize = SESSION_COUNT * WAITERS_PER_SESSION;

#[derive(Clone)]
struct SessionSpec {
    ordinal: usize,
    tenant_id: Uuid,
    workspace_root: PathBuf,
    session_id: Uuid,
    stdout_cursor: u64,
    stderr_cursor: u64,
}

#[cfg(unix)]
struct ProcessGroupSetCleanup {
    process_groups: Vec<(i32, PathBuf)>,
}

#[cfg(unix)]
impl ProcessGroupSetCleanup {
    fn new() -> Self {
        Self {
            process_groups: Vec::new(),
        }
    }

    fn track(&mut self, pid: u32, identity_lock: PathBuf) {
        if let Ok(process_group_id) = i32::try_from(pid) {
            self.process_groups.push((process_group_id, identity_lock));
        }
    }

    fn disarm(&mut self) {
        self.process_groups.clear();
    }
}

#[cfg(unix)]
fn identity_is_held(identity_lock: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(identity_lock)
    else {
        return false;
    };
    // SAFETY: flock operates on a valid owned file descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // SAFETY: the same valid descriptor owns the lock acquired above.
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        false
    } else {
        let raw = std::io::Error::last_os_error().raw_os_error();
        raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN)
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupSetCleanup {
    fn drop(&mut self) {
        for (process_group_id, identity_lock) in self.process_groups.drain(..) {
            if !identity_is_held(&identity_lock) {
                continue;
            }
            // SAFETY: process.start creates a fresh group whose ID equals the
            // returned child PID. The still-held identity lease fences this
            // panic-only cleanup from signalling a recycled process group.
            unsafe {
                libc::kill(-process_group_id, libc::SIGKILL);
            }
        }
    }
}

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("multi-session-wait");
    fs::write(
        &executable,
        "#!/bin/sh\n\
         set -eu\n\
         printf 'ready\\n'\n\
         while IFS= read -r line; do printf 'event:%s\\n' \"$line\"; done\n",
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

fn context(
    tenant_id: Uuid,
    workspace_root: PathBuf,
    cancellation: CancellationToken,
) -> ToolExecutionContext {
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
        timeout: Duration::from_secs(12),
        cancellation,
        requested_at: Utc::now(),
    }
}

fn start_request(ordinal: usize) -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: format!("multi_session_start_{ordinal}"),
            name: "process.start".into(),
            arguments: json!({}),
        },
        effect: ToolEffect::NonIdempotent,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "a".repeat(64),
    }
}

fn wait_request(session: &SessionSpec, waiter: usize) -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: format!("multi_session_wait_{}_{}", session.ordinal, waiter),
            name: PROCESS_WAIT_TOOL.into(),
            arguments: json!({
                "session_id": session.session_id,
                "stdout_cursor": session.stdout_cursor,
                "stderr_cursor": session.stderr_cursor,
                "yield_time_ms": 8_000
            }),
        },
        effect: ToolEffect::Pure,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "b".repeat(64),
    }
}

async fn wait_until_ready(manager: &PersistentProcessSessionManager, spec: &mut SessionSpec) {
    let access = ProcessSessionAccess {
        tenant_id: spec.tenant_id,
        workspace_root: spec.workspace_root.clone(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let output = manager
            .interact(
                &access,
                ProcessSessionInteraction {
                    session_id: spec.session_id,
                    stdout_cursor: 0,
                    stderr_cursor: 0,
                    action: ProcessSessionAction::Poll,
                },
            )
            .await
            .unwrap();
        if output.stdout.contains("ready\n") {
            spec.stdout_cursor = output.stdout_cursor;
            spec.stderr_cursor = output.stderr_cursor;
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session {} never became ready",
            spec.ordinal
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The production break this catches is allowing per-waiter polling, one noisy
/// tenant, or one cancellation wave to monopolize observation across live
/// sessions. Sixty-four real processes must retain one observer each and every
/// tenant/Workspace must receive its own durable output without starvation.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixty_four_sessions_keep_one_thousand_waits_bounded_and_tenant_fair() {
    let state = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let manager = std::sync::Arc::new(
        PersistentProcessSessionManager::new(
            state.path().to_path_buf(),
            executor(trusted.path(), &executable),
            16 * 1024,
        )
        .unwrap(),
    );
    let wait = ProcessSessionToolExecutor::new(manager.clone(), ProcessSessionToolOperation::Wait);
    let tenants = (0..TENANT_COUNT)
        .map(|_| Uuid::now_v7())
        .collect::<Vec<_>>();
    let workspaces = (0..SESSION_COUNT)
        .map(|_| tempfile::tempdir().unwrap())
        .collect::<Vec<_>>();
    let mut sessions = Vec::with_capacity(SESSION_COUNT);
    let mut cleanup = ProcessGroupSetCleanup::new();

    for ordinal in 0..SESSION_COUNT {
        let tenant_id = tenants[ordinal % TENANT_COUNT];
        let workspace_root = workspaces[ordinal].path().canonicalize().unwrap();
        let session_id = Uuid::now_v7();
        let started = manager
            .start(ProcessSessionStartRequest {
                session_id,
                request: start_request(ordinal),
                context: context(tenant_id, workspace_root.clone(), CancellationToken::new()),
                initial_stdin: Vec::new(),
            })
            .await
            .unwrap_or_else(|error| panic!("session {ordinal} failed admission: {error}"));
        cleanup.track(
            started.pid.expect("live session has a pid"),
            state
                .path()
                .join("process-sessions")
                .join(session_id.to_string())
                .join("identity.lock"),
        );
        let mut spec = SessionSpec {
            ordinal,
            tenant_id,
            workspace_root,
            session_id,
            stdout_cursor: 0,
            stderr_cursor: 0,
        };
        wait_until_ready(manager.as_ref(), &mut spec).await;
        sessions.push(spec);
    }

    let cancelled = CancellationToken::new();
    let mut cancelled_waits = JoinSet::new();
    let mut live_waits = JoinSet::new();
    for session in &sessions {
        let cancelled_wait = wait.clone();
        let cancelled_session = session.clone();
        let cancellation = cancelled.clone();
        cancelled_waits.spawn(async move {
            cancelled_wait
                .execute(
                    wait_request(&cancelled_session, 0),
                    context(
                        cancelled_session.tenant_id,
                        cancelled_session.workspace_root.clone(),
                        cancellation,
                    ),
                )
                .await
        });
        for waiter in 1..WAITERS_PER_SESSION {
            let wait = wait.clone();
            let session = session.clone();
            live_waits.spawn(async move {
                let expected = format!("event:session-{}\n", session.ordinal);
                let result = wait
                    .execute(
                        wait_request(&session, waiter),
                        context(
                            session.tenant_id,
                            session.workspace_root.clone(),
                            CancellationToken::new(),
                        ),
                    )
                    .await;
                (session.ordinal, expected, result)
            });
        }
    }

    let registration_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = manager.wait_observation_snapshot();
        if snapshot.active_waiters == TOTAL_WAITERS && snapshot.active_observers == SESSION_COUNT {
            break;
        }
        assert!(
            tokio::time::Instant::now() < registration_deadline,
            "wait registration stalled at {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let observations_before = manager.wait_observation_snapshot().filesystem_observations;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let idle_snapshot = manager.wait_observation_snapshot();
    let observation_delta = idle_snapshot
        .filesystem_observations
        .saturating_sub(observations_before);
    assert!(
        observation_delta <= 512,
        "64 idle sessions performed {observation_delta} filesystem observations in 250ms"
    );

    cancelled.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut cancelled_count = 0;
        while let Some(result) = cancelled_waits.join_next().await {
            let error = result
                .unwrap()
                .expect_err("the explicitly cancelled waiter unexpectedly succeeded");
            assert!(matches!(error, ToolExecutionError::Cancelled));
            cancelled_count += 1;
        }
        assert_eq!(cancelled_count, SESSION_COUNT);
    })
    .await
    .expect("the cancellation wave did not finish promptly");

    let cancellation_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let snapshot = manager.wait_observation_snapshot();
        if snapshot.active_waiters == SESSION_COUNT * (WAITERS_PER_SESSION - 1)
            && snapshot.active_observers == SESSION_COUNT
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < cancellation_deadline,
            "cancelling one waiter per session disturbed siblings: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let wake_started_at = tokio::time::Instant::now();
    let mut writes = JoinSet::new();
    for session in &sessions {
        let manager = manager.clone();
        let session = session.clone();
        writes.spawn(async move {
            manager
                .interact(
                    &ProcessSessionAccess {
                        tenant_id: session.tenant_id,
                        workspace_root: session.workspace_root.clone(),
                    },
                    ProcessSessionInteraction {
                        session_id: session.session_id,
                        stdout_cursor: session.stdout_cursor,
                        stderr_cursor: session.stderr_cursor,
                        action: ProcessSessionAction::Write {
                            bytes: format!("session-{}\n", session.ordinal).into_bytes(),
                        },
                    },
                )
                .await
        });
    }
    while let Some(result) = writes.join_next().await {
        result.unwrap().unwrap();
    }

    let mut completion_latencies = Vec::with_capacity(SESSION_COUNT * (WAITERS_PER_SESSION - 1));
    let mut session_completions = vec![0usize; SESSION_COUNT];
    let mut tenant_completions = [0usize; TENANT_COUNT];
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(joined) = live_waits.join_next().await {
            let (ordinal, expected, result) = joined.unwrap();
            let output = result.unwrap();
            assert!(
                output.content["stdout"]
                    .as_str()
                    .unwrap()
                    .contains(&expected),
                "session {ordinal} received the wrong durable output: {:?}",
                output.content
            );
            completion_latencies.push(wake_started_at.elapsed());
            session_completions[ordinal] += 1;
            tenant_completions[ordinal % TENANT_COUNT] += 1;
        }
    })
    .await
    .expect("not every tenant/Workspace was woken within five seconds");

    assert!(
        session_completions
            .iter()
            .all(|count| *count == WAITERS_PER_SESSION - 1)
    );
    assert!(
        tenant_completions
            .iter()
            .all(|count| *count == (SESSION_COUNT / TENANT_COUNT) * (WAITERS_PER_SESSION - 1))
    );
    completion_latencies.sort_unstable();
    let p50 = completion_latencies[completion_latencies.len() / 2];
    let p95 = completion_latencies
        [(completion_latencies.len() * 95 / 100).min(completion_latencies.len() - 1)];
    let p100 = *completion_latencies.last().unwrap();
    eprintln!(
        "64 sessions / {TOTAL_WAITERS} waits: observations_250ms={observation_delta}, \
         p50={p50:?}, p95={p95:?}, p100={p100:?}"
    );
    assert!(
        p50 < Duration::from_secs(1)
            && p95 < Duration::from_secs(2)
            && p100 < Duration::from_secs(4),
        "wake latency gate failed: p50={p50:?}, p95={p95:?}, p100={p100:?}"
    );

    let retirement_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while manager.wait_observation_snapshot().active_observers != 0 {
        assert!(
            tokio::time::Instant::now() < retirement_deadline,
            "shared observers did not retire: {:?}",
            manager.wait_observation_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    for session in &sessions {
        let close_result = manager
            .interact(
                &ProcessSessionAccess {
                    tenant_id: session.tenant_id,
                    workspace_root: session.workspace_root.clone(),
                },
                ProcessSessionInteraction {
                    session_id: session.session_id,
                    stdout_cursor: session.stdout_cursor,
                    stderr_cursor: session.stderr_cursor,
                    action: ProcessSessionAction::Close,
                },
            )
            .await;
        if let Err(error) = close_result {
            panic!("session {} failed close: {error}", session.ordinal);
        }
    }
    cleanup.disarm();
}
