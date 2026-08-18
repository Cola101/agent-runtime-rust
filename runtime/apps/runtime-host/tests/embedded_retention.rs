use agent_protocol::EventEnvelope;
use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::embedded::{
    RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
    RUNTIME_EVENT_CURSOR_SCHEMA_VERSION, RuntimeControlAction, RuntimeControlCommand,
    RuntimeControlReceipt, RuntimeControlReceiptState, RuntimeEventCursorErrorCode,
    RuntimeEventCursorRequest, RuntimeEventCursorState, RuntimeEventStreamItem,
};
use agent_runtime_host::retention::RuntimeRetentionPolicy;
use agent_runtime_host::{
    LocalEvent, LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunRecord, LocalRunState,
    LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

const RUNS: usize = 1_000;

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Return the short provider response.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            endpoint,
            "retention-model",
            "non-secret-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_024,
            max_cost_cents: 100,
            max_duration_seconds: 30,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

async fn read_http_request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 8 * 1024];
    loop {
        let read = socket.read(&mut scratch).await.expect("provider read");
        assert!(read > 0, "provider request ended before its body");
        bytes.extend_from_slice(&scratch[..read]);
        assert!(bytes.len() <= 1024 * 1024, "provider request is bounded");
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("provider request has Content-Length");
        if bytes.len() >= header_end + content_length {
            return;
        }
    }
}

async fn respond(mut socket: TcpStream) {
    read_http_request(&mut socket).await;
    let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("provider response");
}

fn spawn_provider(
    listener: TcpListener,
    requests: Arc<AtomicUsize>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.expect("provider accept");
                    requests.fetch_add(1, Ordering::SeqCst);
                    connections.spawn(respond(socket));
                }
                _ = &mut shutdown_rx => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    result.expect("provider connection");
                }
            }
        }
        while let Some(result) = connections.join_next().await {
            result.expect("provider connection shutdown");
        }
    });
    (shutdown_tx, task)
}

fn directory_metrics(path: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return (files, bytes);
    };
    for entry in entries {
        let entry = entry.expect("state directory entry");
        let metadata = entry.metadata().expect("state entry metadata");
        if metadata.is_dir() {
            let nested = directory_metrics(&entry.path());
            files += nested.0;
            bytes += nested.1;
        } else if metadata.is_file() {
            files += 1;
            bytes += metadata.len();
        }
    }
    (files, bytes)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: the buffer and count match MACH_TASK_BASIC_INFO.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS, "Mach task_info");
    // SAFETY: KERN_SUCCESS means Mach initialized the structure.
    Some(unsafe { info.assume_init() }.resident_size)
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    Some(kib * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes() -> Option<u64> {
    None
}

fn open_fd_count() -> Option<usize> {
    ["/dev/fd", "/proc/self/fd"]
        .into_iter()
        .find_map(|path| std::fs::read_dir(path).ok().map(Iterator::count))
}

fn admission_limits() -> RuntimeAdmissionLimits {
    RuntimeAdmissionLimits {
        max_active_runs: 1,
        max_active_runs_per_tenant: 1,
        max_active_runs_per_workspace: 1,
        max_queued_runs: 8,
        max_queued_runs_per_tenant: 8,
    }
}

fn churn_retention_policy() -> RuntimeRetentionPolicy {
    RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 32,
        max_run_directories_per_tenant: 64,
        retain_terminal_runs_per_workspace: 16,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 2_000,
        max_run_tombstones_per_tenant: 4_000,
        max_control_tombstones_per_workspace: 2_000,
        max_control_tombstones_per_tenant: 4_000,
        ..RuntimeRetentionPolicy::default()
    }
}

fn one_archive_retention_policy() -> RuntimeRetentionPolicy {
    RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 4,
        max_run_directories_per_tenant: 8,
        retain_terminal_runs_per_workspace: 0,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 16,
        max_run_tombstones_per_tenant: 32,
        max_control_tombstones_per_workspace: 16,
        max_control_tombstones_per_tenant: 32,
        max_event_archive_bytes_per_run: 1024 * 1024,
        max_event_archives_per_workspace: 1,
        max_event_archives_per_tenant: 1,
        max_event_archive_bytes_per_workspace: 1024 * 1024,
        max_event_archive_bytes_per_tenant: 1024 * 1024,
    }
}

fn archive_disabled_retention_policy() -> RuntimeRetentionPolicy {
    RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 4,
        max_run_directories_per_tenant: 8,
        retain_terminal_runs_per_workspace: 0,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 16,
        max_run_tombstones_per_tenant: 32,
        max_control_tombstones_per_workspace: 16,
        max_control_tombstones_per_tenant: 32,
        ..RuntimeRetentionPolicy::default()
    }
}

fn build_runtime_with_policy(
    identity: RuntimeInvocationContext,
    local_config: LocalRuntimeConfig,
    policy: RuntimeRetentionPolicy,
) -> EmbeddedRuntime {
    EmbeddedRuntime::new_with_retention(
        admission_limits(),
        vec![RuntimeProfile {
            invocation: identity,
            config: local_config,
        }],
        policy,
    )
    .expect("bounded embedded Runtime")
}

fn build_runtime(
    identity: RuntimeInvocationContext,
    local_config: LocalRuntimeConfig,
) -> EmbeddedRuntime {
    build_runtime_with_policy(identity, local_config, churn_retention_policy())
}

#[tokio::test]
async fn completed_session_history_survives_hot_run_artifact_retention() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let (shutdown, provider) = spawn_provider(listener, Arc::clone(&requests));
    let identity = invocation();
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        endpoint,
    );
    let mut host = LocalRuntimeHost::start_for_invocation(local_config.clone(), identity)
        .expect("standalone Session host");
    let first = host
        .start_session("first durable Session Turn")
        .await
        .expect("first Session Turn");
    assert_eq!(first.run.status, RunStatus::Succeeded);
    assert!(
        LocalRuntimeHost::read_run_record(state.path(), first.run.run_id)
            .expect("Session Run record read")
            .is_some(),
        "root Session Runs must participate in retention governance"
    );
    host.shutdown().await;
    drop(host);

    let runtime = build_runtime_with_policy(
        identity,
        local_config.clone(),
        one_archive_retention_policy(),
    );
    let report = runtime
        .maintain_retention(identity)
        .expect("Session Run retention");
    assert_eq!(report.tombstoned_runs, 1);
    assert_eq!(report.strongly_referenced_runs, 0);
    assert_eq!(report.event_archives, 1);
    assert!(report.event_archive_bytes > 0);
    assert!(
        !state
            .path()
            .join("runs")
            .join(first.run.run_id.to_string())
            .exists()
    );
    let archived = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id: first.run.run_id,
            after_sequence: 0,
            limit: 256,
        })
        .expect("retired Session Run cold history");
    assert!(
        !archived.events.is_empty(),
        "retention must preserve verified cold Event history before deleting the hot Run"
    );
    assert!(!archived.history_gap);
    assert_eq!(archived.earliest_available_sequence, Some(1));
    assert_eq!(
        archived.events.last().map(|event| event.sequence),
        Some(archived.highest_committed_sequence)
    );
    drop(runtime);

    let mut replacement = LocalRuntimeHost::start_for_invocation(local_config, identity)
        .expect("replacement Session host");
    assert_eq!(
        replacement
            .session_history(
                first.head.session_id,
                first.head.branch_id,
                first.head.generation,
            )
            .expect("retained Session transcript")
            .len(),
        1
    );
    let continued = replacement
        .continue_session(
            first.head.session_id,
            first.head.branch_id,
            first.head.generation,
            "continue after hot Run collection",
        )
        .await
        .expect("Session continues from embedded transcript");
    assert_eq!(continued.run.status, RunStatus::Succeeded);
    assert_eq!(continued.head.turn_count, 2);
    replacement.shutdown().await;
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let _ = shutdown.send(());
    provider.await.expect("provider shutdown");
}

#[tokio::test]
async fn cold_event_history_is_bounded_streamable_and_fails_closed_on_corruption() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        "http://127.0.0.1:9/v1/chat/completions".into(),
    );
    let runtime = build_runtime_with_policy(
        identity,
        local_config.clone(),
        one_archive_retention_policy(),
    );

    let first = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            first,
            "first-cold-history",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("first terminal record");
    write_terminal_event(state.path(), identity, first, "run.succeeded");
    let first_report = runtime
        .maintain_retention(identity)
        .expect("first cold archive");
    assert_eq!(first_report.event_archives, 1);
    assert_eq!(first_report.evicted_event_archives, 0);

    std::thread::sleep(Duration::from_millis(2));
    let second = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            second,
            "second-cold-history",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("second terminal record");
    write_event_sequence(
        state.path(),
        identity,
        second,
        &["run.started", "model.output.delta", "run.succeeded"],
    );
    let second_report = runtime
        .maintain_retention(identity)
        .expect("second cold archive");
    assert_eq!(second_report.event_archives, 1);
    assert_eq!(second_report.evicted_event_archives, 1);

    let retired_gap = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id: first,
            after_sequence: 0,
            limit: 1,
        })
        .expect("evicted archive becomes an explicit gap");
    assert!(retired_gap.events.is_empty());
    assert!(retired_gap.history_gap);
    assert_eq!(retired_gap.earliest_available_sequence, None);

    let archived = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id: second,
            after_sequence: 0,
            limit: 1,
        })
        .expect("newest archive remains readable");
    assert_eq!(archived.events.len(), 1);
    assert!(!archived.history_gap);
    assert_eq!(archived.earliest_available_sequence, Some(1));

    let mut stream = runtime
        .subscribe_events(identity, second, 0, 1)
        .expect("retired archive subscription");
    for sequence in 1..=3 {
        assert!(matches!(
            stream.recv().await.expect("archive event").expect("event"),
            RuntimeEventStreamItem::Event { event, .. } if event.sequence == sequence
        ));
    }
    assert!(matches!(
        stream
            .recv()
            .await
            .expect("archive boundary")
            .expect("boundary"),
        RuntimeEventStreamItem::Boundary {
            history_gap: false,
            state: RuntimeEventCursorState::Retired { .. },
            ..
        }
    ));

    let archive_root = state.path().join("retention").join("event-archives");
    let objects = archive_root.join("objects");
    let object = std::fs::read_dir(&objects)
        .expect("archive objects")
        .next()
        .expect("one archive object")
        .expect("archive entry")
        .path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&archive_root)
                .expect("archive root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(archive_root.join("index.json"))
                .expect("archive index metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&object)
                .expect("archive object metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::write(&object, b"tampered\n").expect("tamper cold archive");
    let error = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id: second,
            after_sequence: 0,
            limit: 1,
        })
        .expect_err("promised cold history corruption cannot become a silent gap");
    assert!(matches!(
        error,
        agent_runtime_host::embedded::EmbeddedRuntimeError::EventCursor(error)
            if error.code == RuntimeEventCursorErrorCode::CorruptLog
    ));

    drop(runtime);
    let replacement =
        build_runtime_with_policy(identity, local_config, archive_disabled_retention_policy());
    let pruned = replacement
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id: second,
            after_sequence: 0,
            limit: 1,
        })
        .expect("disabling the optional tier removes its promise, not terminal evidence");
    assert!(pruned.events.is_empty());
    assert!(pruned.history_gap);
    assert_eq!(
        std::fs::read_dir(&objects)
            .expect("archive object directory")
            .count(),
        0,
        "lowering the opt-in policy must reclaim the old cold object"
    );
}

#[test]
fn oversized_cold_event_history_becomes_an_explicit_gap_without_blocking_retirement() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let mut policy = one_archive_retention_policy();
    policy.max_event_archive_bytes_per_run = 1;
    let runtime = build_runtime_with_policy(
        identity,
        config(
            state.path().to_path_buf(),
            workspace.path().to_path_buf(),
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
        policy,
    );
    let run_id = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            run_id,
            "oversized-cold-history",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("terminal Run record");
    write_terminal_event(state.path(), identity, run_id, "run.succeeded");

    let report = runtime
        .maintain_retention(identity)
        .expect("oversized history does not block safe retirement");
    assert_eq!(report.tombstoned_runs, 1);
    assert_eq!(report.event_archives, 0);
    assert_eq!(report.evicted_event_archives, 0);
    let retired = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id,
            after_sequence: 0,
            limit: 1,
        })
        .expect("retired cursor");
    assert!(retired.events.is_empty());
    assert!(retired.history_gap);
}

#[test]
fn tenant_cold_archive_capacity_is_shared_across_workspace_roots() {
    let first_state = tempfile::tempdir().expect("first state root");
    let first_workspace = tempfile::tempdir().expect("first workspace root");
    let second_state = tempfile::tempdir().expect("second state root");
    let second_workspace = tempfile::tempdir().expect("second workspace root");
    let first = invocation();
    let second = RuntimeInvocationContext {
        tenant_id: first.tenant_id,
        application_id: first.application_id,
        ..invocation()
    };
    let runtime = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    first_state.path().to_path_buf(),
                    first_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    second_state.path().to_path_buf(),
                    second_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
        ],
        one_archive_retention_policy(),
    )
    .expect("tenant Runtime");

    let first_run = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        first_state.path(),
        &record(
            first,
            first_run,
            "first-workspace",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("first Run record");
    write_terminal_event(first_state.path(), first, first_run, "run.succeeded");
    assert_eq!(
        runtime
            .maintain_retention(first)
            .expect("first Workspace retention")
            .event_archives,
        1
    );

    let second_run = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        second_state.path(),
        &record(
            second,
            second_run,
            "second-workspace",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("second Run record");
    write_terminal_event(second_state.path(), second, second_run, "run.succeeded");
    assert_eq!(
        runtime
            .maintain_retention(second)
            .expect("second Workspace retention")
            .event_archives,
        0,
        "one tenant cannot multiply its cold archive cap by registering more Workspaces"
    );

    let first_page = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: first,
            run_id: first_run,
            after_sequence: 0,
            limit: 1,
        })
        .expect("first Workspace archive");
    assert_eq!(first_page.events.len(), 1);
    assert!(!first_page.history_gap);
    let second_page = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: second,
            run_id: second_run,
            after_sequence: 0,
            limit: 1,
        })
        .expect("second Workspace explicit gap");
    assert!(second_page.events.is_empty());
    assert!(second_page.history_gap);
}

fn write_terminal_event(
    state_root: &Path,
    identity: RuntimeInvocationContext,
    run_id: Uuid,
    event_type: &str,
) {
    write_event_sequence(state_root, identity, run_id, &[event_type]);
}

fn write_event_sequence(
    state_root: &Path,
    identity: RuntimeInvocationContext,
    run_id: Uuid,
    event_types: &[&str],
) {
    let session_id = Uuid::now_v7();
    let attempt_id = Uuid::now_v7();
    let mut body = Vec::new();
    for (index, event_type) in event_types.iter().enumerate() {
        let envelope = EventEnvelope::new(
            identity.tenant_id,
            session_id,
            run_id,
            u64::try_from(index + 1).expect("event sequence"),
            attempt_id,
            *event_type,
            serde_json::json!({"status": event_type}),
        );
        let event = LocalEvent {
            event_id: envelope.event_id,
            schema_version: envelope.schema_version,
            tenant_id: identity.tenant_id,
            application_id: identity.application_id,
            workload_identity_id: identity.workload_identity_id,
            workspace_id: identity.workspace_id,
            agent_version_id: identity.agent_version_id,
            model_policy_id: identity.model_policy_id,
            session_id: envelope.session_id,
            sequence: envelope.sequence,
            run_id,
            attempt_id: envelope.attempt_id,
            timestamp: envelope.timestamp,
            trace_id: envelope.trace_id,
            event_type: envelope.event_type,
            payload: envelope.payload,
            digest: envelope.digest,
        };
        body.extend(serde_json::to_vec(&event).expect("event JSON"));
        body.push(b'\n');
    }
    let directory = state_root.join("runs").join(run_id.to_string());
    std::fs::create_dir_all(&directory).expect("Run directory");
    std::fs::write(directory.join("events.jsonl"), body).expect("event log");
}

fn record(
    identity: RuntimeInvocationContext,
    run_id: Uuid,
    input: &str,
    state: LocalRunState,
) -> LocalRunRecord {
    LocalRunRecord {
        store_version: 1,
        tenant_id: identity.tenant_id,
        application_id: identity.application_id,
        workload_identity_id: identity.workload_identity_id,
        workspace_id: identity.workspace_id,
        agent_version_id: identity.agent_version_id,
        model_policy_id: identity.model_policy_id,
        run_id,
        input: input.into(),
        state,
        owner_epoch: 1,
    }
}

#[test]
fn event_cursor_rejects_sequence_gaps_and_missing_current_digests() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let run_id = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            run_id,
            "terminal",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("terminal record");
    write_terminal_event(state.path(), identity, run_id, "run.succeeded");
    let event_path = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl");
    let mut event: LocalEvent = serde_json::from_slice(
        std::fs::read(&event_path)
            .expect("event row")
            .strip_suffix(b"\n")
            .expect("newline"),
    )
    .expect("event JSON");
    event.sequence = 2;
    let mut body = serde_json::to_vec(&event).expect("gap JSON");
    body.push(b'\n');
    std::fs::write(&event_path, body).expect("gap event log");

    let runtime = EmbeddedRuntime::new(
        admission_limits(),
        vec![RuntimeProfile {
            invocation: identity,
            config: config(
                state.path().to_path_buf(),
                workspace.path().to_path_buf(),
                "http://127.0.0.1:9/v1/chat/completions".into(),
            ),
        }],
    )
    .expect("Runtime");
    let request = RuntimeEventCursorRequest {
        schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
        invocation: identity,
        run_id,
        after_sequence: 0,
        limit: 1,
    };
    let error = runtime
        .event_cursor(request.clone())
        .expect_err("sequence gap must fail closed");
    assert!(matches!(
        error,
        agent_runtime_host::embedded::EmbeddedRuntimeError::EventCursor(ref error)
            if error.code == RuntimeEventCursorErrorCode::CorruptLog
    ));

    event.sequence = 1;
    event.digest.clear();
    let mut body = serde_json::to_vec(&event).expect("missing digest JSON");
    body.push(b'\n');
    std::fs::write(&event_path, body).expect("missing digest event log");
    let error = runtime
        .event_cursor(request)
        .expect_err("current event without a digest must fail closed");
    assert!(matches!(
        error,
        agent_runtime_host::embedded::EmbeddedRuntimeError::EventCursor(ref error)
            if error.code == RuntimeEventCursorErrorCode::CorruptLog
    ));
}

#[test]
fn one_workspace_state_root_has_one_live_embedded_runtime_owner() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        "http://127.0.0.1:9/v1/chat/completions".into(),
    );
    let first = build_runtime(identity, local_config.clone());
    let conflict = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        vec![RuntimeProfile {
            invocation: identity,
            config: local_config.clone(),
        }],
        churn_retention_policy(),
    )
    .err()
    .expect("a second live owner is refused");
    assert!(conflict.to_string().contains("another Runtime owner"));
    drop(first);
    build_runtime(identity, local_config);
}

#[tokio::test]
async fn retired_control_command_replays_from_the_compact_ledger_without_side_effects() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let run_id = Uuid::now_v7();
    let input = "completed-before-retention";
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            run_id,
            input,
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("terminal Run record");
    write_terminal_event(state.path(), identity, run_id, "run.succeeded");
    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: identity,
        run_id,
        expected_owner_epoch: 1,
        action: RuntimeControlAction::Resume,
    };
    let command_digest = hex::encode(Sha256::digest(
        serde_json::to_vec(&command).expect("command JSON"),
    ));
    let receipt = RuntimeControlReceipt {
        schema_version: RUNTIME_CONTROL_RECEIPT_SCHEMA_VERSION,
        command_id: command.command_id,
        command_digest,
        invocation: identity,
        run_id,
        expected_owner_epoch: 1,
        action: command.action.clone(),
        state: RuntimeControlReceiptState::Completed,
        applied_owner_epoch: 2,
        run_status: Some(RunStatus::Succeeded),
    };
    let receipt_directory = state.path().join("control-receipts");
    std::fs::create_dir_all(&receipt_directory).expect("receipt directory");
    std::fs::write(
        receipt_directory.join(format!("{}.json", command.command_id)),
        serde_json::to_vec_pretty(&receipt).expect("receipt JSON"),
    )
    .expect("control receipt");
    let runtime = build_runtime_with_policy(
        identity,
        config(
            state.path().to_path_buf(),
            workspace.path().to_path_buf(),
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 4,
            max_run_directories_per_tenant: 8,
            retain_terminal_runs_per_workspace: 0,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 8,
            max_run_tombstones_per_tenant: 16,
            max_control_tombstones_per_workspace: 8,
            max_control_tombstones_per_tenant: 16,
            ..RuntimeRetentionPolicy::default()
        },
    );
    let report = runtime
        .maintain_retention(identity)
        .expect("terminal retention");
    assert_eq!(report.tombstoned_runs, 1);
    assert_eq!(report.tombstoned_control_commands, 1);
    assert!(!state.path().join("runs").join(run_id.to_string()).exists());
    assert!(
        runtime
            .read_terminal_tombstone(identity, run_id)
            .expect("tombstone query")
            .is_some()
    );

    let replay = runtime
        .control(command.clone())
        .await
        .expect("control replay");
    assert_eq!(replay.receipt.state, RuntimeControlReceiptState::Completed);
    assert_eq!(replay.receipt.run_status, Some(RunStatus::Succeeded));
    assert!(replay.outcome.is_none());
    let mut conflict = command;
    conflict.expected_owner_epoch = 2;
    assert!(
        runtime
            .control(conflict)
            .await
            .expect_err("retired command id cannot be rebound")
            .to_string()
            .contains("another command")
    );
}

#[tokio::test]
async fn hard_state_capacity_fails_closed_when_only_unfinished_or_indeterminate_runs_remain() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let identity = invocation();
    let running_id = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(identity, running_id, "running", LocalRunState::Running),
    )
    .expect("running record");
    let indeterminate_id = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        state.path(),
        &record(
            identity,
            indeterminate_id,
            "indeterminate",
            LocalRunState::Finished {
                status: "indeterminate".into(),
            },
        ),
    )
    .expect("indeterminate record");
    write_terminal_event(
        state.path(),
        identity,
        indeterminate_id,
        "run.indeterminate",
    );
    let runtime = build_runtime_with_policy(
        identity,
        config(
            state.path().to_path_buf(),
            workspace.path().to_path_buf(),
            "http://127.0.0.1:9/v1/chat/completions".into(),
        ),
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 2,
            max_run_directories_per_tenant: 4,
            retain_terminal_runs_per_workspace: 1,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 8,
            max_run_tombstones_per_tenant: 16,
            max_control_tombstones_per_workspace: 8,
            max_control_tombstones_per_tenant: 16,
            ..RuntimeRetentionPolicy::default()
        },
    );
    let new_run_id = Uuid::now_v7();
    let error = runtime
        .execute(identity, new_run_id, "must-not-start")
        .await
        .expect_err("hard capacity rejects unsafe collection");
    assert!(error.to_string().contains("capacity is exhausted"));
    assert!(
        runtime
            .read_run_record(identity, new_run_id)
            .expect("new Run lookup")
            .is_none()
    );
    assert!(
        state
            .path()
            .join("runs")
            .join(running_id.to_string())
            .is_dir()
    );
    assert!(
        state
            .path()
            .join("runs")
            .join(indeterminate_id.to_string())
            .is_dir()
    );
}

#[tokio::test]
async fn tenant_capacity_is_enforced_across_workspace_state_roots() {
    let first_state = tempfile::tempdir().expect("first state root");
    let first_workspace = tempfile::tempdir().expect("first workspace root");
    let second_state = tempfile::tempdir().expect("second state root");
    let second_workspace = tempfile::tempdir().expect("second workspace root");
    let first = invocation();
    let generated = invocation();
    let second = RuntimeInvocationContext {
        tenant_id: first.tenant_id,
        application_id: first.application_id,
        ..generated
    };
    let first_running = Uuid::now_v7();
    let second_running = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        first_state.path(),
        &record(
            first,
            first_running,
            "first-running",
            LocalRunState::Running,
        ),
    )
    .expect("first running record");
    LocalRuntimeHost::write_run_record(
        second_state.path(),
        &record(
            second,
            second_running,
            "second-running",
            LocalRunState::Running,
        ),
    )
    .expect("second running record");
    let policy = RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 2,
        max_run_directories_per_tenant: 2,
        retain_terminal_runs_per_workspace: 1,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 4,
        max_run_tombstones_per_tenant: 8,
        max_control_tombstones_per_workspace: 4,
        max_control_tombstones_per_tenant: 8,
        ..RuntimeRetentionPolicy::default()
    };
    let runtime = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    first_state.path().to_path_buf(),
                    first_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    second_state.path().to_path_buf(),
                    second_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
        ],
        policy,
    )
    .expect("multi-Workspace tenant Runtime");
    let rejected_run = Uuid::now_v7();
    let error = runtime
        .execute(first, rejected_run, "must-not-start")
        .await
        .expect_err("tenant aggregate capacity must fail closed");
    assert!(error.to_string().contains("tenant Runtime state capacity"));
    assert!(
        runtime
            .read_run_record(first, rejected_run)
            .expect("rejected Run lookup")
            .is_none()
    );
    assert!(
        first_state
            .path()
            .join("runs")
            .join(first_running.to_string())
            .is_dir()
    );
    assert!(
        second_state
            .path()
            .join("runs")
            .join(second_running.to_string())
            .is_dir()
    );
}

#[tokio::test]
async fn tenant_capacity_can_retire_another_workspaces_terminal_run() {
    let first_state = tempfile::tempdir().expect("first state root");
    let first_workspace = tempfile::tempdir().expect("first workspace root");
    let second_state = tempfile::tempdir().expect("second state root");
    let second_workspace = tempfile::tempdir().expect("second workspace root");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let (shutdown, provider) = spawn_provider(listener, Arc::clone(&requests));
    let first = invocation();
    let generated = invocation();
    let second = RuntimeInvocationContext {
        tenant_id: first.tenant_id,
        application_id: first.application_id,
        ..generated
    };
    let running = Uuid::now_v7();
    let terminal = Uuid::now_v7();
    LocalRuntimeHost::write_run_record(
        first_state.path(),
        &record(first, running, "running", LocalRunState::Running),
    )
    .expect("running record");
    LocalRuntimeHost::write_run_record(
        second_state.path(),
        &record(
            second,
            terminal,
            "terminal",
            LocalRunState::Finished {
                status: "succeeded".into(),
            },
        ),
    )
    .expect("terminal record");
    write_terminal_event(second_state.path(), second, terminal, "run.succeeded");
    let policy = RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 2,
        max_run_directories_per_tenant: 2,
        retain_terminal_runs_per_workspace: 1,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 4,
        max_run_tombstones_per_tenant: 8,
        max_control_tombstones_per_workspace: 4,
        max_control_tombstones_per_tenant: 8,
        ..RuntimeRetentionPolicy::default()
    };
    let runtime = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    first_state.path().to_path_buf(),
                    first_workspace.path().to_path_buf(),
                    endpoint.clone(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    second_state.path().to_path_buf(),
                    second_workspace.path().to_path_buf(),
                    endpoint,
                ),
            },
        ],
        policy,
    )
    .expect("multi-Workspace tenant Runtime");
    let new_run = Uuid::now_v7();
    let outcome = runtime
        .execute(first, new_run, "replace-terminal-capacity")
        .await
        .expect("tenant capacity can retire terminal evidence");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(
        runtime
            .read_terminal_tombstone(second, terminal)
            .expect("terminal tombstone")
            .is_some()
    );
    assert!(
        !second_state
            .path()
            .join("runs")
            .join(terminal.to_string())
            .exists()
    );
    let retired = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: second,
            run_id: terminal,
            after_sequence: 0,
            limit: 1,
        })
        .expect("retired event cursor");
    assert!(retired.events.is_empty());
    assert!(retired.history_gap);
    assert_eq!(retired.earliest_available_sequence, None);
    assert_eq!(retired.highest_committed_sequence, 1);
    assert!(matches!(
        retired.state,
        RuntimeEventCursorState::Retired {
            status: RunStatus::Succeeded,
            terminal_sequence: 1,
            ..
        }
    ));
    let caught_up = runtime
        .event_cursor(RuntimeEventCursorRequest {
            after_sequence: 1,
            ..RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: second,
                run_id: terminal,
                after_sequence: 0,
                limit: 1,
            }
        })
        .expect("caught-up retired cursor");
    assert!(!caught_up.history_gap);
    let mut retired_stream = runtime
        .subscribe_events(second, terminal, 0, 1)
        .expect("retired stream boundary");
    let boundary = retired_stream
        .recv()
        .await
        .expect("retired stream item")
        .expect("retired stream result");
    assert!(matches!(
        boundary,
        RuntimeEventStreamItem::Boundary {
            history_gap: true,
            state: RuntimeEventCursorState::Retired { .. },
            ..
        }
    ));
    assert!(
        first_state
            .path()
            .join("runs")
            .join(running.to_string())
            .is_dir()
    );
    assert!(
        first_state
            .path()
            .join("runs")
            .join(new_run.to_string())
            .is_dir()
    );
    let _ = shutdown.send(());
    provider.await.expect("provider shutdown");
}

#[test]
fn preexisting_tenant_tombstone_overflow_is_rejected_at_startup() {
    let first_state = tempfile::tempdir().expect("first state root");
    let first_workspace = tempfile::tempdir().expect("first workspace root");
    let second_state = tempfile::tempdir().expect("second state root");
    let second_workspace = tempfile::tempdir().expect("second workspace root");
    let first = invocation();
    let generated = invocation();
    let second = RuntimeInvocationContext {
        tenant_id: first.tenant_id,
        application_id: first.application_id,
        ..generated
    };
    for (root, identity) in [(first_state.path(), first), (second_state.path(), second)] {
        let run_id = Uuid::now_v7();
        LocalRuntimeHost::write_run_record(
            root,
            &record(
                identity,
                run_id,
                "terminal",
                LocalRunState::Finished {
                    status: "succeeded".into(),
                },
            ),
        )
        .expect("terminal record");
        write_terminal_event(root, identity, run_id, "run.succeeded");
    }
    let profiles = || {
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    first_state.path().to_path_buf(),
                    first_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    second_state.path().to_path_buf(),
                    second_workspace.path().to_path_buf(),
                    "http://127.0.0.1:9/v1/chat/completions".into(),
                ),
            },
        ]
    };
    let initial = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        profiles(),
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 2,
            max_run_directories_per_tenant: 4,
            retain_terminal_runs_per_workspace: 0,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 4,
            max_run_tombstones_per_tenant: 8,
            max_control_tombstones_per_workspace: 4,
            max_control_tombstones_per_tenant: 8,
            ..RuntimeRetentionPolicy::default()
        },
    )
    .expect("initial Runtime");
    assert_eq!(
        initial
            .maintain_retention(first)
            .expect("first retention")
            .tombstoned_runs,
        1
    );
    assert_eq!(
        initial
            .maintain_retention(second)
            .expect("second retention")
            .tombstoned_runs,
        1
    );
    drop(initial);

    let error = EmbeddedRuntime::new_with_retention(
        admission_limits(),
        profiles(),
        RuntimeRetentionPolicy {
            max_run_directories_per_workspace: 2,
            max_run_directories_per_tenant: 4,
            retain_terminal_runs_per_workspace: 1,
            min_terminal_age: Duration::ZERO,
            max_run_tombstones_per_workspace: 1,
            max_run_tombstones_per_tenant: 1,
            max_control_tombstones_per_workspace: 1,
            max_control_tombstones_per_tenant: 1,
            ..RuntimeRetentionPolicy::default()
        },
    )
    .err()
    .expect("aggregate tombstone overflow must fail closed");
    assert!(
        error
            .to_string()
            .contains("tenant Runtime tombstone capacity")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_tenant_multi_workspace_churn_keeps_every_root_bounded_and_recoverable() {
    const TENANTS: usize = 4;
    const WORKSPACES_PER_TENANT: usize = 3;
    const RUNS_PER_WORKSPACE: usize = 32;

    let started = Instant::now();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let (shutdown, provider) = spawn_provider(listener, Arc::clone(&requests));
    let mut state_roots = Vec::new();
    let mut workspace_roots = Vec::new();
    let mut profiles = Vec::new();
    for _ in 0..TENANTS {
        let tenant_id = Uuid::now_v7();
        for _ in 0..WORKSPACES_PER_TENANT {
            let state = tempfile::tempdir().expect("state root");
            let workspace = tempfile::tempdir().expect("workspace root");
            let identity = RuntimeInvocationContext {
                tenant_id,
                ..invocation()
            };
            profiles.push(RuntimeProfile {
                invocation: identity,
                config: config(
                    state.path().to_path_buf(),
                    workspace.path().to_path_buf(),
                    endpoint.clone(),
                ),
            });
            state_roots.push(state);
            workspace_roots.push(workspace);
        }
    }
    let retention_policy = RuntimeRetentionPolicy {
        max_run_directories_per_workspace: 12,
        max_run_directories_per_tenant: 36,
        retain_terminal_runs_per_workspace: 6,
        min_terminal_age: Duration::ZERO,
        max_run_tombstones_per_workspace: 64,
        max_run_tombstones_per_tenant: 192,
        max_control_tombstones_per_workspace: 64,
        max_control_tombstones_per_tenant: 192,
        ..RuntimeRetentionPolicy::default()
    };
    let limits = RuntimeAdmissionLimits {
        max_active_runs: 4,
        max_active_runs_per_tenant: 2,
        max_active_runs_per_workspace: 1,
        max_queued_runs: 32,
        max_queued_runs_per_tenant: 16,
    };
    let runtime = EmbeddedRuntime::new_with_retention(limits, profiles.clone(), retention_policy)
        .expect("multi-tenant Runtime");
    let mut first = None;
    for ordinal in 0..RUNS_PER_WORKSPACE {
        for profile in &profiles {
            let run_id = Uuid::now_v7();
            let input = format!(
                "tenant-workspace-churn-{}-{ordinal}",
                profile.invocation.workspace_id
            );
            first.get_or_insert((profile.invocation, run_id, input.clone()));
            let outcome = runtime
                .execute(profile.invocation, run_id, &input)
                .await
                .expect("multi-tenant churn Run");
            assert_eq!(outcome.status, RunStatus::Succeeded);
        }
    }
    for profile in &profiles {
        let report = runtime
            .maintain_retention(profile.invocation)
            .expect("Workspace maintenance");
        assert_eq!(report.run_directories_after, 6);
        assert_eq!(report.total_run_tombstones, RUNS_PER_WORKSPACE - 6);
        assert_eq!(report.unmanaged_run_directories, 0);
        assert_eq!(report.strongly_referenced_runs, 0);
        assert!(report.terminal_ledger_bytes < 256 * 1024);
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        TENANTS * WORKSPACES_PER_TENANT * RUNS_PER_WORKSPACE
    );
    drop(runtime);

    let replacement =
        EmbeddedRuntime::new_with_retention(limits, profiles.clone(), retention_policy)
            .expect("replacement multi-tenant Runtime");
    let (first_invocation, first_run_id, first_input) = first.expect("first Run binding");
    replacement
        .execute(first_invocation, first_run_id, &first_input)
        .await
        .expect_err("replacement must preserve the exact retired Run fence");
    for profile in &profiles {
        assert_eq!(
            replacement
                .maintain_retention(profile.invocation)
                .expect("replacement maintenance")
                .run_directories_after,
            6
        );
    }
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(90), "the churn took {elapsed:?}");
    drop(replacement);
    drop(workspace_roots);
    drop(state_roots);
    let _ = shutdown.send(());
    provider.await.expect("provider shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded() {
    let started = Instant::now();
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace root");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let (shutdown, provider) = spawn_provider(listener, Arc::clone(&requests));
    let identity = invocation();
    let local_config = config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        endpoint,
    );
    let runtime = build_runtime(identity, local_config.clone());
    let baseline_rss = resident_bytes();
    let baseline_fds = open_fd_count();
    let mut peak_rss = baseline_rss;
    let mut peak_fds = baseline_fds;
    let mut first = None;

    for index in 0..RUNS {
        let run_id = Uuid::now_v7();
        let input = format!("retention-churn-{index}");
        if first.is_none() {
            first = Some((run_id, input.clone()));
        }
        let outcome = runtime
            .execute(identity, run_id, &input)
            .await
            .expect("churn Run");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        if (index + 1) % 100 == 0 {
            if let Some(current) = resident_bytes() {
                peak_rss = Some(peak_rss.unwrap_or(0).max(current));
            }
            if let Some(current) = open_fd_count() {
                peak_fds = Some(peak_fds.unwrap_or(0).max(current));
            }
        }
    }

    let scan_started = Instant::now();
    let report = runtime
        .maintain_retention(identity)
        .expect("final maintenance");
    let scan_elapsed = scan_started.elapsed();
    assert_eq!(report.run_directories_after, 16);
    assert_eq!(report.total_run_tombstones, RUNS - 16);
    assert_eq!(report.total_control_tombstones, 0);
    assert_eq!(report.unmanaged_run_directories, 0);
    assert!(report.terminal_ledger_bytes < 2 * 1024 * 1024);
    assert!(
        scan_elapsed < Duration::from_secs(2),
        "the recovery scan took {scan_elapsed:?}"
    );

    let (files, state_bytes) = directory_metrics(state.path());
    assert!(
        files <= report.run_directories_after * 6 + 4,
        "each retained Run and the compact ledger have a fixed file budget"
    );
    assert!(state_bytes < 8 * 1024 * 1024, "bounded local state");
    assert_eq!(requests.load(Ordering::SeqCst), RUNS);

    let (first_run, first_input) = first.expect("first Run binding");
    let exact_replay = runtime
        .execute(identity, first_run, &first_input)
        .await
        .expect_err("retired Run must not execute again");
    assert!(exact_replay.to_string().contains("replay was refused"));
    let conflicting_replay = runtime
        .execute(identity, first_run, "different input")
        .await
        .expect_err("retired id cannot bind new input");
    assert!(conflicting_replay.to_string().contains("conflicts"));
    assert_eq!(requests.load(Ordering::SeqCst), RUNS);

    drop(runtime);
    let replacement = build_runtime(identity, local_config);
    let replacement_scan = Instant::now();
    let replacement_report = replacement
        .maintain_retention(identity)
        .expect("replacement maintenance");
    assert_eq!(replacement_report.run_directories_after, 16);
    assert_eq!(replacement_report.total_run_tombstones, RUNS - 16);
    let replacement_elapsed = replacement_scan.elapsed();
    assert!(
        replacement_elapsed < Duration::from_secs(2),
        "the replacement's recovery scan took {replacement_elapsed:?}"
    );
    replacement
        .execute(identity, first_run, &first_input)
        .await
        .expect_err("replacement must preserve retired Run fence");
    assert_eq!(requests.load(Ordering::SeqCst), RUNS);

    let final_fds = open_fd_count();
    // Printed before the bounds are checked, not after. Every number this test
    // is about is in this line, and an `assert!` that fires first takes the
    // whole line with it -- which left "elapsed exceeded 180s" with no way to
    // know whether it was 181 seconds or six hundred, and the same for the two
    // resource bounds above it.
    println!(
        "retention_churn_metrics runs={RUNS} hot_run_directories={} tombstones={} ledger_bytes={} state_files={files} state_bytes={state_bytes} recovery_scan_ms={} rss_baseline_bytes={} rss_peak_bytes={} fd_baseline={} fd_peak={} fd_final={} elapsed_ms={}",
        report.run_directories_after,
        report.total_run_tombstones,
        report.terminal_ledger_bytes,
        scan_elapsed.as_millis(),
        baseline_rss.unwrap_or(0),
        peak_rss.unwrap_or(0),
        baseline_fds.unwrap_or(0),
        peak_fds.unwrap_or(0),
        final_fds.unwrap_or(0),
        started.elapsed().as_millis(),
    );

    // And each bound says what it measured, because a captured stdout is not
    // shown for a test that passes and is easy to miss for one that fails.
    if let (Some(baseline), Some(peak)) = (baseline_rss, peak_rss) {
        let grew = peak.saturating_sub(baseline);
        assert!(
            grew <= 512 * 1024 * 1024,
            "resident memory grew by {grew} bytes over a baseline of {baseline}"
        );
    }
    if let (Some(baseline), Some(peak)) = (baseline_fds, peak_fds) {
        let grew = peak.saturating_sub(baseline);
        assert!(grew <= 32, "open descriptors peaked {grew} above a baseline of {baseline}");
    }
    if let (Some(baseline), Some(final_count)) = (baseline_fds, final_fds) {
        assert!(
            final_count <= baseline + 8,
            "{final_count} descriptors remain open against a baseline of {baseline}"
        );
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(180),
        "{RUNS} runs took {:?}, which is the wall clock this bound is about -- \
         the metrics line above says where it went",
        elapsed,
    );

    let _ = shutdown.send(());
    provider.await.expect("provider shutdown");
}
