//! Native M1 capacity gate for the protocol-neutral embedded Runtime.
//!
//! The test holds 1,000 in-flight calls while admitting only 32 Hosts/provider
//! connections. It deliberately cancels queued and active work, and leaves a
//! capacity-one durable event subscriber unread until its Run finishes.

use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RuntimeControlAction,
    RuntimeControlCommand, RuntimeControlReceiptState, RuntimeEventCursorState,
    RuntimeEventStreamItem, RuntimeProfile,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunState, LocalRuntimeConfig,
    LocalToolConsent,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use uuid::Uuid;

const TENANTS: usize = 20;
const WORKSPACES_PER_TENANT: usize = 10;
const RUNS_PER_WORKSPACE: usize = 5;
const TOTAL_RUNS: usize = TENANTS * WORKSPACES_PER_TENANT * RUNS_PER_WORKSPACE;
const MAX_ACTIVE: usize = 32;
const MAX_ACTIVE_PER_TENANT: usize = 2;
const ABORTED_QUEUED_RUNS: usize = 500;
const CANCELLED_ACTIVE_RUNS: usize = 16;

#[derive(Clone)]
struct CapacityCase {
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
    marker: String,
}

struct ProviderEvidence {
    start_order: Mutex<Vec<(usize, String)>>,
    completed: AtomicUsize,
    disconnected_before_release: AtomicUsize,
}

fn invocation(
    tenant_id: Uuid,
    application_id: Uuid,
    workspace_id: Uuid,
) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id,
        application_id,
        workload_identity_id: Uuid::now_v7(),
        workspace_id,
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: &str) -> LocalRuntimeConfig {
    let mut model_routing = LocalModelRoutingConfig::single_openai_compatible(
        endpoint.to_owned(),
        "capacity-model",
        "non-secret-capacity-key",
    );
    model_routing.health_policy.max_same_provider_attempts = 1;
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Return the tenant-bound capacity marker.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing,
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_024,
            max_cost_cents: 10,
            max_duration_seconds: 120,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

async fn read_http_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 8 * 1024];
    loop {
        let read = socket.read(&mut scratch).await.expect("provider read");
        assert!(read > 0, "provider request ended before its body");
        bytes.extend_from_slice(&scratch[..read]);
        assert!(bytes.len() <= 256 * 1024, "provider request is bounded");
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
            return String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
                .expect("provider request body is UTF-8 JSON");
        }
    }
}

async fn wait_for_release_or_disconnect(
    socket: &mut TcpStream,
    release: &mut watch::Receiver<bool>,
) -> bool {
    let mut ignored = [0_u8; 64];
    loop {
        if *release.borrow() {
            return true;
        }
        tokio::select! {
            changed = release.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            read = socket.read(&mut ignored) => {
                match read {
                    Ok(0) | Err(_) => return false,
                    Ok(_) => {}
                }
            }
        }
    }
}

async fn handle_provider_request(
    mut socket: TcpStream,
    cases: Arc<HashMap<String, usize>>,
    evidence: Arc<ProviderEvidence>,
    mut release: watch::Receiver<bool>,
) {
    let request = read_http_request(&mut socket).await;
    let (marker, tenant_index) = cases
        .iter()
        .find(|(marker, _)| request.contains(marker.as_str()))
        .map(|(marker, tenant_index)| (marker.clone(), *tenant_index))
        .expect("each provider request carries one capacity marker");
    evidence
        .start_order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((tenant_index, marker.clone()));
    if !wait_for_release_or_disconnect(&mut socket, &mut release).await {
        evidence
            .disconnected_before_release
            .fetch_add(1, Ordering::SeqCst);
        return;
    }
    let body = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"finished {marker}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if socket.write_all(response.as_bytes()).await.is_ok() {
        evidence.completed.fetch_add(1, Ordering::SeqCst);
    }
}

fn spawn_provider(
    listener: TcpListener,
    cases: Arc<HashMap<String, usize>>,
    evidence: Arc<ProviderEvidence>,
    release: watch::Receiver<bool>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.expect("provider accept");
                    requests.spawn(handle_provider_request(
                        socket,
                        Arc::clone(&cases),
                        Arc::clone(&evidence),
                        release.clone(),
                    ));
                }
                _ = &mut shutdown_rx => break,
                Some(result) = requests.join_next(), if !requests.is_empty() => {
                    result.expect("provider request task");
                }
            }
        }
        requests.abort_all();
        while requests.join_next().await.is_some() {}
    });
    (shutdown_tx, task)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: the output buffer and count exactly match MACH_TASK_BASIC_INFO.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS, "Mach task_info");
    // SAFETY: KERN_SUCCESS guarantees initialization.
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
    Some(kib * 1_024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes() -> Option<u64> {
    None
}

fn open_fd_count() -> Option<usize> {
    for path in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(entries) = std::fs::read_dir(path) {
            return Some(entries.count());
        }
    }
    None
}

async fn sample_resources(mut stop: watch::Receiver<bool>) -> (Option<u64>, Option<usize>) {
    let mut peak_rss = resident_bytes();
    let mut peak_fds = open_fd_count();
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return (peak_rss, peak_fds);
                }
            }
            () = tokio::time::sleep(Duration::from_millis(2)) => {
                if let Some(current) = resident_bytes() {
                    peak_rss = Some(peak_rss.unwrap_or(0).max(current));
                }
                if let Some(current) = open_fd_count() {
                    peak_fds = Some(peak_fds.unwrap_or(0).max(current));
                }
            }
        }
    }
}

async fn wait_for_capacity(runtime: &EmbeddedRuntime, owners: usize, active: usize, queued: usize) {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let snapshot = runtime.runtime_snapshot();
            if snapshot.active_execution_owners == owners
                && snapshot.admission.active_runs == active
                && snapshot.admission.queued_runs == queued
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Runtime reached the expected bounded capacity state");
}

async fn wait_for_provider_starts(evidence: &ProviderEvidence, expected: usize) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if evidence
                .start_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                >= expected
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("provider observed the expected active wave");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_thousand_inflight_runs_keep_only_thirty_two_hosts_active_and_cancel_cleanly() {
    let started_at = Instant::now();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let tenant_ids = (0..TENANTS).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let application_ids = (0..TENANTS).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let mut state_roots = Vec::with_capacity(TENANTS * WORKSPACES_PER_TENANT);
    let mut workspace_roots = Vec::with_capacity(TENANTS * WORKSPACES_PER_TENANT);
    let mut profiles = Vec::with_capacity(TENANTS * WORKSPACES_PER_TENANT);
    let mut profile_invocations = Vec::with_capacity(TENANTS * WORKSPACES_PER_TENANT);

    for tenant_index in 0..TENANTS {
        for _ in 0..WORKSPACES_PER_TENANT {
            let state = tempfile::tempdir().expect("state root");
            let workspace = tempfile::tempdir().expect("workspace root");
            let identity = invocation(
                tenant_ids[tenant_index],
                application_ids[tenant_index],
                Uuid::now_v7(),
            );
            profiles.push(RuntimeProfile {
                invocation: identity,
                config: config(
                    state.path().to_path_buf(),
                    workspace
                        .path()
                        .canonicalize()
                        .expect("canonical workspace"),
                    &endpoint,
                ),
            });
            profile_invocations.push(identity);
            state_roots.push(state);
            workspace_roots.push(workspace);
        }
    }

    let mut cases = Vec::with_capacity(TOTAL_RUNS);
    let mut provider_cases = HashMap::with_capacity(TOTAL_RUNS);
    // The first twenty calls intentionally cover every tenant before the rest
    // of the 1,000-call backlog is admitted.
    for run_round in 0..RUNS_PER_WORKSPACE {
        for workspace_index in 0..WORKSPACES_PER_TENANT {
            for tenant_index in 0..TENANTS {
                let invocation =
                    profile_invocations[tenant_index * WORKSPACES_PER_TENANT + workspace_index];
                let marker = format!("capacity-t{tenant_index}-w{workspace_index}-r{run_round}");
                provider_cases.insert(marker.clone(), tenant_index);
                cases.push(CapacityCase {
                    invocation,
                    run_id: Uuid::now_v7(),
                    marker,
                });
            }
        }
    }
    assert_eq!(cases.len(), TOTAL_RUNS);

    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: MAX_ACTIVE,
                max_active_runs_per_tenant: MAX_ACTIVE_PER_TENANT,
                max_active_runs_per_workspace: 1,
                max_queued_runs: TOTAL_RUNS,
                max_queued_runs_per_tenant: WORKSPACES_PER_TENANT * RUNS_PER_WORKSPACE,
            },
            profiles,
        )
        .expect("capacity Runtime"),
    );
    assert!(
        runtime
            .subscribe_events(cases[0].invocation, cases[0].run_id, 0, 0)
            .is_err(),
        "zero-capacity event subscriptions must fail closed"
    );
    assert!(
        runtime
            .subscribe_events(
                cases[0].invocation,
                cases[0].run_id,
                0,
                agent_runtime_host::embedded::EMBEDDED_EVENT_SUBSCRIPTION_MAX_CAPACITY + 1,
            )
            .is_err(),
        "oversized event subscriptions must fail closed"
    );
    let evidence = Arc::new(ProviderEvidence {
        start_order: Mutex::new(Vec::with_capacity(TOTAL_RUNS)),
        completed: AtomicUsize::new(0),
        disconnected_before_release: AtomicUsize::new(0),
    });
    let (release_tx, release_rx) = watch::channel(false);
    let (shutdown_tx, provider_task) = spawn_provider(
        listener,
        Arc::new(provider_cases),
        Arc::clone(&evidence),
        release_rx,
    );
    let baseline_rss = resident_bytes();
    let baseline_fds = open_fd_count();
    let (sampler_stop_tx, sampler_stop_rx) = watch::channel(false);
    let resource_sampler = tokio::spawn(sample_resources(sampler_stop_rx));

    let mut tasks = Vec::with_capacity(TOTAL_RUNS);
    for case in cases.iter().take(TENANTS).cloned() {
        let execution_runtime = Arc::clone(&runtime);
        let marker = case.marker.clone();
        tasks.push(Some(tokio::spawn(async move {
            execution_runtime
                .execute(case.invocation, case.run_id, &marker)
                .await
        })));
    }
    wait_for_provider_starts(&evidence, TENANTS).await;
    for case in cases.iter().skip(TENANTS).cloned() {
        let execution_runtime = Arc::clone(&runtime);
        let marker = case.marker.clone();
        tasks.push(Some(tokio::spawn(async move {
            execution_runtime
                .execute(case.invocation, case.run_id, &marker)
                .await
        })));
    }
    wait_for_capacity(&runtime, TOTAL_RUNS, MAX_ACTIVE, TOTAL_RUNS - MAX_ACTIVE).await;
    wait_for_provider_starts(&evidence, MAX_ACTIVE).await;

    let mut slow_events = runtime
        .subscribe_events(cases[0].invocation, cases[0].run_id, 0, 1)
        .expect("bounded durable event subscription");
    assert_eq!(slow_events.capacity(), 1);
    let subscription_snapshot = runtime.runtime_snapshot();
    assert_eq!(subscription_snapshot.active_event_subscriptions, 1);
    assert_eq!(subscription_snapshot.buffered_event_slots, 1);

    let initial = runtime.runtime_snapshot();
    assert_eq!(initial.admission.peak_active_runs, MAX_ACTIVE);
    assert_eq!(initial.admission.peak_queued_runs, TOTAL_RUNS - MAX_ACTIVE);
    assert_eq!(initial.admission.peak_active_runs_per_tenant, 2);
    assert_eq!(initial.admission.peak_active_runs_per_workspace, 1);
    let first_wave = evidence
        .start_order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .take(MAX_ACTIVE)
        .map(|(tenant_index, _)| *tenant_index)
        .collect::<HashSet<_>>();
    assert_eq!(first_wave.len(), TENANTS, "every tenant starts in wave one");

    let mut aborted = HashSet::with_capacity(ABORTED_QUEUED_RUNS);
    for (index, case) in cases.iter().enumerate() {
        if aborted.len() == ABORTED_QUEUED_RUNS {
            break;
        }
        if runtime
            .read_run_record(case.invocation, case.run_id)
            .expect("queued Run record lookup")
            .is_none()
        {
            let task = tasks[index].take().expect("queued task remains owned");
            task.abort();
            assert!(
                task.await
                    .expect_err("queued task was aborted")
                    .is_cancelled()
            );
            aborted.insert(index);
        }
    }
    assert_eq!(aborted.len(), ABORTED_QUEUED_RUNS);
    wait_for_capacity(
        &runtime,
        TOTAL_RUNS - ABORTED_QUEUED_RUNS,
        MAX_ACTIVE,
        TOTAL_RUNS - MAX_ACTIVE - ABORTED_QUEUED_RUNS,
    )
    .await;

    let mut cancelled = Vec::with_capacity(CANCELLED_ACTIVE_RUNS);
    for (index, case) in cases.iter().enumerate() {
        if cancelled.len() == CANCELLED_ACTIVE_RUNS {
            break;
        }
        if index == 0 || aborted.contains(&index) {
            continue;
        }
        let Some(record) = runtime
            .read_run_record(case.invocation, case.run_id)
            .expect("active Run record lookup")
        else {
            continue;
        };
        if record.state != LocalRunState::Running {
            continue;
        }
        let command = RuntimeControlCommand {
            schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: Uuid::now_v7(),
            invocation: case.invocation,
            run_id: case.run_id,
            expected_owner_epoch: record.owner_epoch,
            action: RuntimeControlAction::Cancel {
                reason: "capacity cancellation storm".into(),
            },
        };
        let accepted = runtime
            .control(command.clone())
            .await
            .expect("active cancellation accepted");
        assert!(matches!(
            accepted.receipt.state,
            RuntimeControlReceiptState::Accepted | RuntimeControlReceiptState::Completed
        ));
        cancelled.push((index, command));
    }
    assert_eq!(cancelled.len(), CANCELLED_ACTIVE_RUNS);
    for (index, command) in &cancelled {
        let outcome = tasks[*index]
            .take()
            .expect("cancelled task remains owned")
            .await
            .expect("cancelled task joins")
            .expect("cancelled Run outcome");
        assert_eq!(outcome.status, RunStatus::Cancelled);
        let completed = runtime
            .control(command.clone())
            .await
            .expect("cancellation receipt replay");
        assert_eq!(
            completed.receipt.state,
            RuntimeControlReceiptState::Completed
        );
        assert_eq!(completed.receipt.run_status, Some(RunStatus::Cancelled));
    }
    wait_for_capacity(
        &runtime,
        TOTAL_RUNS - ABORTED_QUEUED_RUNS - CANCELLED_ACTIVE_RUNS,
        MAX_ACTIVE,
        TOTAL_RUNS - MAX_ACTIVE - ABORTED_QUEUED_RUNS - CANCELLED_ACTIVE_RUNS,
    )
    .await;
    wait_for_provider_starts(&evidence, MAX_ACTIVE + CANCELLED_ACTIVE_RUNS).await;
    let promoted_wave = evidence
        .start_order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .skip(MAX_ACTIVE)
        .take(CANCELLED_ACTIVE_RUNS)
        .map(|(tenant_index, _)| *tenant_index)
        .collect::<HashSet<_>>();
    assert!(
        promoted_wave.len() >= 8,
        "cancelled capacity is redistributed across backlogged tenants"
    );

    release_tx.send(true).expect("release provider wave");
    let cancelled_indices = cancelled
        .iter()
        .map(|(index, _)| *index)
        .collect::<HashSet<_>>();
    let mut succeeded = 0_usize;
    tokio::time::timeout(Duration::from_secs(120), async {
        for (index, task) in tasks.iter_mut().enumerate() {
            if aborted.contains(&index) || cancelled_indices.contains(&index) {
                assert!(task.is_none());
                continue;
            }
            let outcome = task
                .take()
                .expect("successful task remains owned")
                .await
                .expect("successful task joins")
                .expect("successful Run outcome");
            assert_eq!(outcome.status, RunStatus::Succeeded);
            succeeded += 1;
        }
    })
    .await
    .expect("remaining Runs finish within the native capacity deadline");
    assert_eq!(
        succeeded,
        TOTAL_RUNS - ABORTED_QUEUED_RUNS - CANCELLED_ACTIVE_RUNS
    );

    let mut observed_events = Vec::new();
    let mut observed_boundary = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = slow_events.recv().await {
            match item.expect("bounded subscription item") {
                RuntimeEventStreamItem::Event { event, .. } => observed_events.push(*event),
                RuntimeEventStreamItem::Boundary {
                    state,
                    history_gap,
                    next_after_sequence,
                    ..
                } => {
                    assert!(!history_gap);
                    assert_eq!(
                        next_after_sequence,
                        observed_events.last().map_or(0, |event| event.sequence)
                    );
                    observed_boundary = Some(state);
                    return;
                }
            }
        }
        panic!("event subscription closed before an explicit boundary");
    })
    .await
    .expect("slow event subscriber catches up from durable storage");
    assert!(observed_events.len() >= 4);
    assert!(
        observed_events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1)
    );
    assert_eq!(
        observed_events
            .last()
            .map(|event| event.event_type.as_str()),
        Some("run.succeeded")
    );
    assert_eq!(
        observed_boundary,
        Some(RuntimeEventCursorState::Terminal {
            status: RunStatus::Succeeded
        })
    );
    let reconnect_cursor = observed_events[1].sequence;
    let expected_replay = observed_events
        .iter()
        .filter(|event| event.sequence > reconnect_cursor)
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    let mut reconnected = runtime
        .subscribe_events(cases[0].invocation, cases[0].run_id, reconnect_cursor, 2)
        .expect("reconnected bounded event subscription");
    let mut replayed = Vec::new();
    let mut replay_boundary = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(item) = reconnected.recv().await {
            match item.expect("reconnected item") {
                RuntimeEventStreamItem::Event { event, .. } => replayed.push(event.event_id),
                RuntimeEventStreamItem::Boundary { state, .. } => {
                    replay_boundary = Some(state);
                    return;
                }
            }
        }
        panic!("reconnected subscription closed before an explicit boundary");
    })
    .await
    .expect("reconnected subscriber resumes from its exclusive cursor");
    assert_eq!(replayed, expected_replay);
    assert_eq!(replay_boundary, observed_boundary);
    let concurrent_subscription_snapshot = runtime.runtime_snapshot();
    assert_eq!(
        concurrent_subscription_snapshot.active_event_subscriptions,
        2
    );
    assert_eq!(concurrent_subscription_snapshot.buffered_event_slots, 3);
    drop(reconnected);
    drop(slow_events);

    sampler_stop_tx.send(true).expect("stop resource sampler");
    let (peak_rss, peak_fds) = resource_sampler.await.expect("resource sampler");
    if let (Some(baseline), Some(peak)) = (baseline_rss, peak_rss) {
        assert!(
            peak.saturating_sub(baseline) <= 512 * 1024 * 1024,
            "1,000 in-flight Runs exceeded the 512 MiB incremental RSS budget: baseline={baseline}, peak={peak}"
        );
    }
    if let (Some(baseline), Some(peak)) = (baseline_fds, peak_fds) {
        assert!(
            peak.saturating_sub(baseline) <= 160,
            "32 admitted Hosts exceeded the descriptor budget: baseline={baseline}, peak={peak}"
        );
    }
    let final_fds = open_fd_count();
    if let (Some(baseline), Some(final_count)) = (baseline_fds, final_fds) {
        assert!(
            final_count <= baseline + 16,
            "completed capacity storm retained descriptors: baseline={baseline}, final={final_count}"
        );
    }
    let final_snapshot = runtime.runtime_snapshot();
    assert_eq!(
        final_snapshot.registered_profiles,
        TENANTS * WORKSPACES_PER_TENANT
    );
    assert_eq!(final_snapshot.active_execution_owners, 0);
    assert_eq!(final_snapshot.peak_active_execution_owners, TOTAL_RUNS);
    assert_eq!(final_snapshot.active_event_subscriptions, 0);
    assert_eq!(final_snapshot.buffered_event_slots, 0);
    assert_eq!(final_snapshot.peak_active_event_subscriptions, 2);
    assert_eq!(final_snapshot.peak_buffered_event_slots, 3);
    assert_eq!(final_snapshot.admission.active_runs, 0);
    assert_eq!(final_snapshot.admission.queued_runs, 0);
    assert_eq!(final_snapshot.admission.peak_active_runs, MAX_ACTIVE);
    assert_eq!(
        final_snapshot.admission.peak_queued_runs,
        TOTAL_RUNS - MAX_ACTIVE
    );
    assert_eq!(
        evidence.completed.load(Ordering::SeqCst),
        TOTAL_RUNS - ABORTED_QUEUED_RUNS - CANCELLED_ACTIVE_RUNS
    );
    assert_eq!(
        evidence.disconnected_before_release.load(Ordering::SeqCst),
        CANCELLED_ACTIVE_RUNS
    );
    assert!(
        started_at.elapsed() < Duration::from_secs(120),
        "capacity gate exceeded the M1 Pro native acceptance window"
    );
    eprintln!(
        "capacity_metrics inflight={TOTAL_RUNS} profiles={} tenants={TENANTS} active_peak={} queued_peak={} first_wave_tenants={} promoted_tenants={} aborted_queued={ABORTED_QUEUED_RUNS} cancelled_active={CANCELLED_ACTIVE_RUNS} succeeded={succeeded} event_count={} event_subscription_peak={} event_buffer_slots_peak={} rss_baseline_bytes={} rss_peak_bytes={} fd_baseline={} fd_peak={} fd_final={} elapsed_ms={}",
        final_snapshot.registered_profiles,
        final_snapshot.admission.peak_active_runs,
        final_snapshot.admission.peak_queued_runs,
        first_wave.len(),
        promoted_wave.len(),
        observed_events.len(),
        final_snapshot.peak_active_event_subscriptions,
        final_snapshot.peak_buffered_event_slots,
        baseline_rss.unwrap_or(0),
        peak_rss.unwrap_or(0),
        baseline_fds.unwrap_or(0),
        peak_fds.unwrap_or(0),
        final_fds.unwrap_or(0),
        started_at.elapsed().as_millis(),
    );
    let _ = shutdown_tx.send(());
    provider_task.await.expect("provider shutdown");
    drop((runtime, state_roots, workspace_roots));
}
