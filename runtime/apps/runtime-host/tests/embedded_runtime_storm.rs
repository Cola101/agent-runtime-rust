use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION, RuntimeControlAction,
    RuntimeControlCommand, RuntimeControlReceiptState, RuntimeProfile,
};
use agent_runtime_host::{
    LocalApprovalDecision, LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalRunState,
    LocalRuntimeConfig, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

const TENANTS: usize = 10;
const PROFILES_PER_TENANT: usize = 10;
const TOTAL_PROFILES: usize = TENANTS * PROFILES_PER_TENANT;
const MAX_ACTIVE: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StormMode {
    Complete,
    ApprovalAllow,
    ApprovalDeny,
    Cancel,
    Resume,
}

struct StormRun {
    tenant_index: usize,
    mode: StormMode,
    first_request_seen: Notify,
    attempts: AtomicUsize,
}

#[derive(Clone)]
struct StormCase {
    marker: String,
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
    run: Arc<StormRun>,
}

fn mode_for(profile_index: usize) -> StormMode {
    match profile_index {
        0..=5 => StormMode::Complete,
        6 => StormMode::ApprovalAllow,
        7 => StormMode::ApprovalDeny,
        8 => StormMode::Cancel,
        9 => StormMode::Resume,
        _ => unreachable!("profile index is bounded"),
    }
}

fn invocation(tenant_id: Uuid, application_id: Uuid) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id,
        application_id,
        workload_identity_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

fn trusted_tool_binary() -> Option<PathBuf> {
    let mut current = std::env::current_exe().ok()?;
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    endpoint: &str,
    trusted_tool: &Path,
) -> LocalRuntimeConfig {
    let mut model_routing = LocalModelRoutingConfig::single_openai_compatible(
        endpoint.to_owned(),
        "storm-model",
        "non-secret-test-key",
    );
    model_routing.health_policy.max_same_provider_attempts = 2;
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Obey the tenant-bound storm scenario.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing,
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: Some(trusted_tool.to_path_buf()),
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 90,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

fn text_turn(marker: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"finished {marker}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn tool_call_turn(marker: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-{marker}\",\"type\":\"function\",\"function\":{{\"name\":\"workspace.read_text\",\"arguments\":\"{{\\\"path\\\":\\\"README.txt\\\"}}\"}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

async fn read_http_request(socket: &mut TcpStream) -> String {
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
            return String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
                .expect("provider request body is UTF-8 JSON");
        }
    }
}

async fn write_sse(socket: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("provider response");
}

async fn wait_for_release(release: &mut watch::Receiver<bool>) {
    while !*release.borrow() {
        release.changed().await.expect("storm release sender");
    }
}

async fn hold_until_client_closes(socket: &mut TcpStream) {
    let mut sink = [0_u8; 256];
    loop {
        match socket.read(&mut sink).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

async fn handle_provider_request(
    mut socket: TcpStream,
    runs: Arc<HashMap<String, Arc<StormRun>>>,
    mut release: watch::Receiver<bool>,
    first_start_order: Arc<Mutex<Vec<usize>>>,
) {
    let request = read_http_request(&mut socket).await;
    let (marker, run) = runs
        .iter()
        .find(|(marker, _)| request.contains(marker.as_str()))
        .map(|(marker, run)| (marker.clone(), Arc::clone(run)))
        .expect("each provider request carries one storm marker");
    let attempt = run.attempts.fetch_add(1, Ordering::SeqCst) + 1;
    if attempt == 1 {
        first_start_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(run.tenant_index);
        run.first_request_seen.notify_one();
    }
    wait_for_release(&mut release).await;
    match (run.mode, attempt) {
        (StormMode::Complete, 1) => write_sse(&mut socket, &text_turn(&marker)).await,
        (StormMode::ApprovalAllow | StormMode::ApprovalDeny, 1) => {
            write_sse(&mut socket, &tool_call_turn(&marker)).await;
        }
        (StormMode::ApprovalAllow | StormMode::ApprovalDeny, 2) | (StormMode::Resume, 2) => {
            write_sse(&mut socket, &text_turn(&marker)).await
        }
        (StormMode::Cancel, 1) | (StormMode::Resume, 1) => {
            hold_until_client_closes(&mut socket).await;
        }
        _ => panic!("unexpected provider attempt {attempt} for {marker}"),
    }
}

fn spawn_provider(
    listener: TcpListener,
    runs: Arc<HashMap<String, Arc<StormRun>>>,
    release: watch::Receiver<bool>,
    first_start_order: Arc<Mutex<Vec<usize>>>,
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
                        Arc::clone(&runs),
                        release.clone(),
                        Arc::clone(&first_start_order),
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

async fn wait_for_state(
    runtime: &EmbeddedRuntime,
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
    predicate: impl Fn(&LocalRunState) -> bool,
) -> agent_runtime_host::LocalRunRecord {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(record) = runtime
                .read_run_record(invocation, run_id)
                .expect("read Run record")
                && predicate(&record.state)
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Run reached its expected state")
}

async fn execute_case(
    runtime: Arc<EmbeddedRuntime>,
    case: StormCase,
    mut release: watch::Receiver<bool>,
) {
    match case.run.mode {
        StormMode::Complete => {
            let outcome = runtime
                .execute(case.invocation, case.run_id, &case.marker)
                .await
                .expect("complete Run");
            assert_eq!(outcome.status, RunStatus::Succeeded);
        }
        StormMode::ApprovalAllow | StormMode::ApprovalDeny => {
            let parked = runtime
                .execute(case.invocation, case.run_id, &case.marker)
                .await
                .expect("approval Run parks");
            let approval = parked.pending_approval.expect("pending approval");
            let record = runtime
                .read_run_record(case.invocation, case.run_id)
                .expect("approval record")
                .expect("durable approval record");
            let command = RuntimeControlCommand {
                schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
                command_id: Uuid::now_v7(),
                invocation: case.invocation,
                run_id: case.run_id,
                expected_owner_epoch: record.owner_epoch,
                action: RuntimeControlAction::DecideApproval {
                    target_run_id: approval.target_run_id,
                    approval_id: approval.approval_id,
                    binding_digest: approval.binding_digest,
                    decision: if case.run.mode == StormMode::ApprovalAllow {
                        LocalApprovalDecision::AllowOnce
                    } else {
                        LocalApprovalDecision::Deny
                    },
                },
            };
            let applied = runtime
                .control(command.clone())
                .await
                .expect("approval control");
            assert_eq!(applied.receipt.state, RuntimeControlReceiptState::Completed);
            assert_eq!(applied.receipt.run_status, Some(RunStatus::Succeeded));
            let repeated = runtime.control(command).await.expect("approval replay");
            assert_eq!(repeated.receipt, applied.receipt);
            assert!(repeated.outcome.is_none());
        }
        StormMode::Cancel => {
            let execution_runtime = Arc::clone(&runtime);
            let marker = case.marker.clone();
            let execution = tokio::spawn(async move {
                execution_runtime
                    .execute(case.invocation, case.run_id, &marker)
                    .await
            });
            case.run.first_request_seen.notified().await;
            wait_for_release(&mut release).await;
            let record = wait_for_state(&runtime, case.invocation, case.run_id, |state| {
                *state == LocalRunState::Running
            })
            .await;
            let command = RuntimeControlCommand {
                schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
                command_id: Uuid::now_v7(),
                invocation: case.invocation,
                run_id: case.run_id,
                expected_owner_epoch: record.owner_epoch,
                action: RuntimeControlAction::Cancel {
                    reason: "storm cancellation".into(),
                },
            };
            let accepted = runtime
                .control(command.clone())
                .await
                .expect("cancel control");
            assert!(matches!(
                accepted.receipt.state,
                RuntimeControlReceiptState::Accepted | RuntimeControlReceiptState::Completed
            ));
            let outcome = execution
                .await
                .expect("cancel task")
                .expect("cancel outcome");
            assert_eq!(outcome.status, RunStatus::Cancelled);
            let completed = runtime.control(command).await.expect("cancel replay");
            assert_eq!(
                completed.receipt.state,
                RuntimeControlReceiptState::Completed
            );
            assert_eq!(completed.receipt.run_status, Some(RunStatus::Cancelled));
        }
        StormMode::Resume => {
            let execution_runtime = Arc::clone(&runtime);
            let marker = case.marker.clone();
            let execution = tokio::spawn(async move {
                execution_runtime
                    .execute(case.invocation, case.run_id, &marker)
                    .await
            });
            case.run.first_request_seen.notified().await;
            wait_for_release(&mut release).await;
            let record = wait_for_state(&runtime, case.invocation, case.run_id, |state| {
                *state == LocalRunState::Running
            })
            .await;
            execution.abort();
            let _ = execution.await;
            let command = RuntimeControlCommand {
                schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
                command_id: Uuid::now_v7(),
                invocation: case.invocation,
                run_id: case.run_id,
                expected_owner_epoch: record.owner_epoch,
                action: RuntimeControlAction::Resume,
            };
            let resumed = runtime
                .control(command.clone())
                .await
                .expect("resume control");
            assert_eq!(resumed.receipt.state, RuntimeControlReceiptState::Completed);
            assert_eq!(resumed.receipt.run_status, Some(RunStatus::Succeeded));
            let repeated = runtime.control(command).await.expect("resume replay");
            assert_eq!(repeated.receipt, resumed.receipt);
            assert!(repeated.outcome.is_none());
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: `info` is sized for the requested flavor and `count` reports that
    // exact size. Mach initializes the structure before KERN_SUCCESS.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS, "Mach task_info");
    // SAFETY: KERN_SUCCESS above guarantees the output was initialized.
    let info = unsafe { info.assume_init() };
    Some(info.resident_size)
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
    for path in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(entries) = std::fs::read_dir(path) {
            return Some(entries.count());
        }
    }
    None
}

async fn sample_process_resources(mut stop: watch::Receiver<bool>) -> (Option<u64>, Option<usize>) {
    let mut peak_rss = resident_bytes();
    let mut peak_fds = open_fd_count();
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
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
    (peak_rss, peak_fds)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_profiles_keep_mixed_execution_control_and_resources_bounded() {
    let started_at = Instant::now();
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let tenant_ids = (0..TENANTS).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let application_ids = (0..TENANTS).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let mut state_roots = Vec::with_capacity(TOTAL_PROFILES);
    let mut workspaces = Vec::with_capacity(TOTAL_PROFILES);
    let mut profiles = Vec::with_capacity(TOTAL_PROFILES);
    let mut cases = Vec::with_capacity(TOTAL_PROFILES);
    let mut runs = HashMap::with_capacity(TOTAL_PROFILES);

    // Interleave tenants at submission time, so every tenant is queued before
    // the first active wave is released.
    for profile_index in 0..PROFILES_PER_TENANT {
        for tenant_index in 0..TENANTS {
            let state = tempfile::tempdir().expect("state root");
            let workspace = tempfile::tempdir().expect("workspace");
            std::fs::write(
                workspace.path().join("README.txt"),
                "tenant storm evidence\n",
            )
            .expect("workspace fixture");
            let identity = invocation(tenant_ids[tenant_index], application_ids[tenant_index]);
            let mode = mode_for(profile_index);
            let marker = format!("storm-t{tenant_index}-p{profile_index}-{mode:?}");
            let run = Arc::new(StormRun {
                tenant_index,
                mode,
                first_request_seen: Notify::new(),
                attempts: AtomicUsize::new(0),
            });
            profiles.push(RuntimeProfile {
                invocation: identity,
                config: config(
                    state.path().to_path_buf(),
                    workspace
                        .path()
                        .canonicalize()
                        .expect("canonical workspace"),
                    &endpoint,
                    &trusted_tool,
                ),
            });
            let case = StormCase {
                marker: marker.clone(),
                invocation: identity,
                run_id: Uuid::now_v7(),
                run: Arc::clone(&run),
            };
            runs.insert(marker, run);
            cases.push(case);
            state_roots.push(state);
            workspaces.push(workspace);
        }
    }

    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: MAX_ACTIVE,
                max_active_runs_per_tenant: 2,
                max_active_runs_per_workspace: 1,
                max_queued_runs: TOTAL_PROFILES,
                max_queued_runs_per_tenant: PROFILES_PER_TENANT,
            },
            profiles,
        )
        .expect("100-profile Runtime"),
    );
    let baseline_rss = resident_bytes();
    let baseline_fds = open_fd_count();
    let (sampler_stop_tx, sampler_stop_rx) = watch::channel(false);
    let resource_sampler = tokio::spawn(sample_process_resources(sampler_stop_rx));
    let (release_tx, release_rx) = watch::channel(false);
    let first_start_order = Arc::new(Mutex::new(Vec::with_capacity(TOTAL_PROFILES)));
    let (shutdown_tx, provider_task) = spawn_provider(
        listener,
        Arc::new(runs),
        release_rx.clone(),
        Arc::clone(&first_start_order),
    );

    let mut tasks = Vec::with_capacity(TOTAL_PROFILES);
    for case in cases.clone() {
        tasks.push(tokio::spawn(execute_case(
            Arc::clone(&runtime),
            case,
            release_rx.clone(),
        )));
    }

    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let snapshot = runtime.runtime_snapshot();
            if snapshot.active_execution_owners == TOTAL_PROFILES
                && snapshot.admission.active_runs == MAX_ACTIVE
                && snapshot.admission.queued_runs == TOTAL_PROFILES - MAX_ACTIVE
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("all profiles enter the bounded active/queued set");

    release_tx.send(true).expect("release storm");
    tokio::time::timeout(Duration::from_secs(120), async {
        for task in tasks {
            task.await.expect("storm task");
        }
    })
    .await
    .expect("mixed storm completes within its native deadline");
    sampler_stop_tx.send(true).expect("stop resource sampler");
    let (peak_rss, peak_fds) = resource_sampler.await.expect("resource sampler");
    if let (Some(baseline), Some(peak)) = (baseline_rss, peak_rss) {
        assert!(
            peak.saturating_sub(baseline) <= 384 * 1024 * 1024,
            "100 queued/active Runs exceeded the 384 MiB native RSS budget: baseline={baseline}, peak={peak}"
        );
    }
    if let (Some(baseline), Some(peak)) = (baseline_fds, peak_fds) {
        assert!(
            peak.saturating_sub(baseline) <= 64,
            "bounded active Runs opened too many descriptors: baseline={baseline}, peak={peak}"
        );
    }

    let final_snapshot = runtime.runtime_snapshot();
    assert_eq!(final_snapshot.registered_profiles, TOTAL_PROFILES);
    assert_eq!(final_snapshot.active_execution_owners, 0);
    assert_eq!(final_snapshot.peak_active_execution_owners, TOTAL_PROFILES);
    assert_eq!(final_snapshot.admission.active_runs, 0);
    assert_eq!(final_snapshot.admission.queued_runs, 0);
    assert_eq!(final_snapshot.admission.active_tenants, 0);
    assert_eq!(final_snapshot.admission.active_workspaces, 0);
    assert_eq!(final_snapshot.admission.queued_tenants, 0);
    assert_eq!(final_snapshot.admission.peak_active_runs, MAX_ACTIVE);
    assert_eq!(
        final_snapshot.admission.peak_queued_runs,
        TOTAL_PROFILES - MAX_ACTIVE
    );
    assert!(final_snapshot.admission.peak_active_runs_per_tenant <= 2);
    assert_eq!(final_snapshot.admission.peak_active_runs_per_workspace, 1);

    let order = first_start_order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(order.len(), TOTAL_PROFILES);
    let mut first_forty = [0_usize; TENANTS];
    for tenant in order.iter().take(40) {
        first_forty[*tenant] += 1;
    }
    assert!(
        first_forty.iter().all(|admitted| *admitted >= 2),
        "every backlogged tenant must make progress in the first forty starts: {first_forty:?}"
    );

    let mut total_receipts = 0;
    let mut total_provider_attempts = 0;
    for case in &cases {
        let record = runtime
            .read_run_record(case.invocation, case.run_id)
            .expect("final record read")
            .expect("final Run record");
        match case.run.mode {
            StormMode::Cancel => assert!(matches!(record.state, LocalRunState::Cancelled { .. })),
            _ => assert!(matches!(
                record.state,
                LocalRunState::Finished { ref status } if status == "succeeded"
            )),
        }
        let receipts = runtime
            .list_control_receipts(case.invocation, case.run_id)
            .expect("control receipt list");
        let expected_receipts = usize::from(case.run.mode != StormMode::Complete);
        assert_eq!(receipts.len(), expected_receipts);
        assert!(receipts.iter().all(|receipt| {
            receipt.state == RuntimeControlReceiptState::Completed
                && receipt.run_status
                    == Some(if case.run.mode == StormMode::Cancel {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Succeeded
                    })
        }));
        total_receipts += receipts.len();
        let attempts = case.run.attempts.load(Ordering::SeqCst);
        let expected_attempts = match case.run.mode {
            StormMode::Complete | StormMode::Cancel => 1,
            StormMode::ApprovalAllow | StormMode::ApprovalDeny | StormMode::Resume => 2,
        };
        assert_eq!(attempts, expected_attempts);
        total_provider_attempts += attempts;
    }
    assert_eq!(total_receipts, 40);
    assert_eq!(total_provider_attempts, 130);

    let final_fds = open_fd_count();
    if let (Some(baseline), Some(final_count)) = (baseline_fds, final_fds) {
        assert!(
            final_count <= baseline + 16,
            "completed storm retained file descriptors: baseline={baseline}, final={final_count}"
        );
    }
    assert!(
        started_at.elapsed() < Duration::from_secs(120),
        "storm exceeded the M1 Pro native acceptance window"
    );
    eprintln!(
        "storm_metrics profiles={TOTAL_PROFILES} tenants={TENANTS} active_peak={} queued_peak={} owner_peak={} receipts={total_receipts} provider_attempts={total_provider_attempts} rss_baseline_bytes={} rss_peak_bytes={} fd_baseline={} fd_peak={} fd_final={} elapsed_ms={}",
        final_snapshot.admission.peak_active_runs,
        final_snapshot.admission.peak_queued_runs,
        final_snapshot.peak_active_execution_owners,
        baseline_rss.unwrap_or(0),
        peak_rss.unwrap_or(0),
        baseline_fds.unwrap_or(0),
        peak_fds.unwrap_or(0),
        final_fds.unwrap_or(0),
        started_at.elapsed().as_millis(),
    );
    let _ = shutdown_tx.send(());
    provider_task.await.expect("provider shutdown");
    drop((state_roots, workspaces));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_queue_rejects_resume_before_acceptance_and_the_same_command_remains_retryable() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let mut state_roots = Vec::new();
    let mut workspaces = Vec::new();
    let mut profiles = Vec::new();
    let mut cases = Vec::new();
    let mut runs = HashMap::new();
    for index in 0..3 {
        let state = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let identity = invocation(Uuid::now_v7(), Uuid::now_v7());
        let marker = format!("saturated-resume-{index}");
        let run = Arc::new(StormRun {
            tenant_index: index,
            mode: StormMode::Resume,
            first_request_seen: Notify::new(),
            attempts: AtomicUsize::new(0),
        });
        profiles.push(RuntimeProfile {
            invocation: identity,
            config: config(
                state.path().to_path_buf(),
                workspace
                    .path()
                    .canonicalize()
                    .expect("canonical workspace"),
                &endpoint,
                &trusted_tool,
            ),
        });
        let case = StormCase {
            marker: marker.clone(),
            invocation: identity,
            run_id: Uuid::now_v7(),
            run: Arc::clone(&run),
        };
        runs.insert(marker, run);
        cases.push(case);
        state_roots.push(state);
        workspaces.push(workspace);
    }
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 1,
                max_queued_runs_per_tenant: 1,
            },
            profiles,
        )
        .expect("saturated Runtime"),
    );
    let (_release_tx, release_rx) = watch::channel(true);
    let (shutdown_tx, provider_task) = spawn_provider(
        listener,
        Arc::new(runs),
        release_rx,
        Arc::new(Mutex::new(Vec::new())),
    );

    let lost = cases[0].clone();
    let lost_runtime = Arc::clone(&runtime);
    let lost_marker = lost.marker.clone();
    let lost_execution = tokio::spawn(async move {
        lost_runtime
            .execute(lost.invocation, lost.run_id, &lost_marker)
            .await
    });
    lost.run.first_request_seen.notified().await;
    let lost_record = wait_for_state(&runtime, lost.invocation, lost.run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    lost_execution.abort();
    let _ = lost_execution.await;

    let active = cases[1].clone();
    let active_runtime = Arc::clone(&runtime);
    let active_marker = active.marker.clone();
    let active_execution = tokio::spawn(async move {
        active_runtime
            .execute(active.invocation, active.run_id, &active_marker)
            .await
    });
    active.run.first_request_seen.notified().await;

    let queued = cases[2].clone();
    let queued_runtime = Arc::clone(&runtime);
    let queued_marker = queued.marker.clone();
    let queued_execution = tokio::spawn(async move {
        queued_runtime
            .execute(queued.invocation, queued.run_id, &queued_marker)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.admission_snapshot().queued_runs != 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("one Run fills the queue");

    let rejected_run_id = Uuid::now_v7();
    let rejected = runtime
        .execute(lost.invocation, rejected_run_id, "queue-overflow-run")
        .await
        .expect_err("full queue rejects a new Run before durable acceptance");
    assert!(rejected.to_string().contains("global queue"));
    assert!(
        runtime
            .read_run_record(lost.invocation, rejected_run_id)
            .expect("rejected Run lookup")
            .is_none(),
        "a Run rejected by admission must not leave false Running state"
    );

    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: lost.invocation,
        run_id: lost.run_id,
        expected_owner_epoch: lost_record.owner_epoch,
        action: RuntimeControlAction::Resume,
    };
    let error = runtime
        .control(command.clone())
        .await
        .expect_err("full queue rejects recovery admission");
    assert!(error.to_string().contains("global queue"));
    assert!(
        runtime
            .read_control_receipt(lost.invocation, command.command_id)
            .expect("receipt read")
            .is_none(),
        "queue rejection must happen before command acceptance"
    );
    let unchanged = runtime
        .read_run_record(lost.invocation, lost.run_id)
        .expect("unchanged Run read")
        .expect("unchanged Run record");
    assert_eq!(unchanged.owner_epoch, lost_record.owner_epoch);
    assert!(matches!(unchanged.state, LocalRunState::Running));

    queued_execution.abort();
    let _ = queued_execution.await;
    active_execution.abort();
    let _ = active_execution.await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = runtime.runtime_snapshot();
            if snapshot.active_execution_owners == 0
                && snapshot.admission.active_runs == 0
                && snapshot.admission.queued_runs == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("aborted fixtures release every owner and slot");
    let resumed = runtime
        .control(command.clone())
        .await
        .expect("same resume command is retryable after capacity returns");
    assert_eq!(resumed.receipt.state, RuntimeControlReceiptState::Completed);
    assert_eq!(resumed.receipt.run_status, Some(RunStatus::Succeeded));
    assert_eq!(
        runtime
            .read_run_record(lost.invocation, lost.run_id)
            .expect("resumed Run read")
            .expect("resumed Run record")
            .owner_epoch,
        lost_record.owner_epoch + 1
    );
    let _ = shutdown_tx.send(());
    provider_task.await.expect("provider shutdown");
    drop((state_roots, workspaces));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_queue_rejects_cancel_before_acceptance_and_the_same_command_remains_retryable() {
    let trusted_tool = trusted_tool_binary().expect("agent-trusted-workspace-tool must be built");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("provider address")
    );
    let mut state_roots = Vec::new();
    let mut workspaces = Vec::new();
    let mut profiles = Vec::new();
    let mut cases = Vec::new();
    let mut runs = HashMap::new();
    for index in 0..3 {
        let state = tempfile::tempdir().expect("state root");
        let workspace = tempfile::tempdir().expect("workspace");
        let identity = invocation(Uuid::now_v7(), Uuid::now_v7());
        let marker = format!("saturated-cancel-{index}");
        let run = Arc::new(StormRun {
            tenant_index: index,
            mode: StormMode::Resume,
            first_request_seen: Notify::new(),
            attempts: AtomicUsize::new(0),
        });
        profiles.push(RuntimeProfile {
            invocation: identity,
            config: config(
                state.path().to_path_buf(),
                workspace
                    .path()
                    .canonicalize()
                    .expect("canonical workspace"),
                &endpoint,
                &trusted_tool,
            ),
        });
        let case = StormCase {
            marker: marker.clone(),
            invocation: identity,
            run_id: Uuid::now_v7(),
            run: Arc::clone(&run),
        };
        runs.insert(marker, run);
        cases.push(case);
        state_roots.push(state);
        workspaces.push(workspace);
    }
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 1,
                max_queued_runs_per_tenant: 1,
            },
            profiles,
        )
        .expect("saturated Runtime"),
    );
    let (_release_tx, release_rx) = watch::channel(true);
    let (shutdown_tx, provider_task) = spawn_provider(
        listener,
        Arc::new(runs),
        release_rx,
        Arc::new(Mutex::new(Vec::new())),
    );

    let lost = cases[0].clone();
    let lost_runtime = Arc::clone(&runtime);
    let lost_marker = lost.marker.clone();
    let lost_execution = tokio::spawn(async move {
        lost_runtime
            .execute(lost.invocation, lost.run_id, &lost_marker)
            .await
    });
    lost.run.first_request_seen.notified().await;
    let lost_record = wait_for_state(&runtime, lost.invocation, lost.run_id, |state| {
        *state == LocalRunState::Running
    })
    .await;
    lost_execution.abort();
    let _ = lost_execution.await;

    let active = cases[1].clone();
    let active_runtime = Arc::clone(&runtime);
    let active_marker = active.marker.clone();
    let active_execution = tokio::spawn(async move {
        active_runtime
            .execute(active.invocation, active.run_id, &active_marker)
            .await
    });
    active.run.first_request_seen.notified().await;

    let queued = cases[2].clone();
    let queued_runtime = Arc::clone(&runtime);
    let queued_marker = queued.marker.clone();
    let queued_execution = tokio::spawn(async move {
        queued_runtime
            .execute(queued.invocation, queued.run_id, &queued_marker)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.admission_snapshot().queued_runs != 1 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("one Run fills the queue");

    let command = RuntimeControlCommand {
        schema_version: RUNTIME_CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: Uuid::now_v7(),
        invocation: lost.invocation,
        run_id: lost.run_id,
        expected_owner_epoch: lost_record.owner_epoch,
        action: RuntimeControlAction::Cancel {
            reason: "saturated cancellation".into(),
        },
    };
    let error = runtime
        .control(command.clone())
        .await
        .expect_err("full queue rejects cancellation recovery admission");
    assert!(error.to_string().contains("global queue"));
    assert!(
        runtime
            .read_control_receipt(lost.invocation, command.command_id)
            .expect("receipt read")
            .is_none(),
        "queue rejection must happen before cancellation acceptance"
    );
    let unchanged = runtime
        .read_run_record(lost.invocation, lost.run_id)
        .expect("unchanged Run read")
        .expect("unchanged Run record");
    assert_eq!(unchanged.owner_epoch, lost_record.owner_epoch);
    assert!(matches!(unchanged.state, LocalRunState::Running));

    queued_execution.abort();
    let _ = queued_execution.await;
    active_execution.abort();
    let _ = active_execution.await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = runtime.runtime_snapshot();
            if snapshot.active_execution_owners == 0
                && snapshot.admission.active_runs == 0
                && snapshot.admission.queued_runs == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("aborted fixtures release every owner and slot");
    let cancelled = runtime
        .control(command.clone())
        .await
        .expect("same cancel command is retryable after capacity returns");
    assert_eq!(
        cancelled.receipt.state,
        RuntimeControlReceiptState::Completed
    );
    assert_eq!(cancelled.receipt.run_status, Some(RunStatus::Cancelled));
    assert!(matches!(
        runtime
            .read_run_record(lost.invocation, lost.run_id)
            .expect("cancelled Run read")
            .expect("cancelled Run record")
            .state,
        LocalRunState::Cancelled { .. }
    ));
    let _ = shutdown_tx.send(());
    provider_task.await.expect("provider shutdown");
    drop((state_roots, workspaces));
}
