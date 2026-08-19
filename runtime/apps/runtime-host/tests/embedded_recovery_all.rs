//! Multi-tenant startup recovery is a Runtime lifecycle, not adapter glue.
//!
//! A single-profile daemon already scans its one invocation. An embedded host
//! can register many immutable profiles, so requiring Java, CLI, or a future
//! GUI to enumerate those profiles and sequence recovery would leak Runtime
//! ownership semantics into every adapter. These tests exercise one aggregate
//! entry point and require one corrupt tenant to remain isolated.

use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION, RuntimeEventCursorRequest,
    RuntimeEventCursorState, RuntimeProfile,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig,
    LocalRuntimeHost, LocalToolConsent,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

fn invocation(tenant_id: Uuid) -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id,
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        model_policy_id: Uuid::now_v7(),
    }
}

fn limits() -> RuntimeAdmissionLimits {
    RuntimeAdmissionLimits {
        max_active_runs: 2,
        max_active_runs_per_tenant: 1,
        max_active_runs_per_workspace: 1,
        max_queued_runs: 2,
        max_queued_runs_per_tenant: 1,
    }
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    let mut model_routing = LocalModelRoutingConfig {
        allowed_regions: BTreeSet::from(["local".into()]),
        data_class: DataClass::Internal,
        max_cost_per_million_tokens_micros: 1_000_000,
        health_policy: Default::default(),
        candidates: vec![LocalProviderConfig {
            id: "loopback".into(),
            protocol: ProviderProtocol::OpenAiCompatible,
            endpoint,
            model: "test-model".into(),
            api_key: "test-key".into(),
            region: "local".into(),
            accepted_data_classes: BTreeSet::from([DataClass::Internal]),
            capabilities: BTreeSet::from([Capability::Text]),
            healthy: true,
            latency_ms: 1,
            cost_per_million_tokens_micros: 1,
            response_timeout_ms: 10_000,
            stream_idle_timeout_ms: 10_000,
            max_output_tokens: None,
        }],
    };
    model_routing.health_policy.max_same_provider_attempts = 2;
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Recover only this immutable invocation.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing,
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_000,
            max_cost_cents: 100,
            max_duration_seconds: 120,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

async fn spawn_recoverable_provider(
    answer: &'static str,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&calls);
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first request");
        let mut request = vec![0_u8; 64 * 1024];
        let _ = first.read(&mut request).await;
        observed_calls.fetch_add(1, Ordering::SeqCst);
        let _ = first_seen_tx.send(());
        let mut eof = [0_u8; 1];
        let _ = first.read(&mut eof).await;
        drop(first);

        let (mut replacement, _) = listener.accept().await.expect("replacement request");
        let _ = replacement.read(&mut request).await;
        observed_calls.fetch_add(1, Ordering::SeqCst);
        let body = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        replacement
            .write_all(response.as_bytes())
            .await
            .expect("reply");
    });
    (endpoint, first_seen_rx, calls, server)
}

async fn leave_orphaned_running_run(
    profile: RuntimeProfile,
    run_id: Uuid,
    first_seen: tokio::sync::oneshot::Receiver<()>,
) {
    let invocation = profile.invocation;
    tokio::task::spawn_blocking(move || {
        let thread_runtime = tokio::runtime::Runtime::new().expect("Runtime");
        thread_runtime.block_on(async move {
            let runtime = Arc::new(EmbeddedRuntime::new(limits(), vec![profile]).expect("first"));
            runtime
                .execute_detached(invocation, run_id, "finish after replacement".into())
                .await
                .expect("durable acceptance");
            first_seen.await.expect("first Provider request");
            let page = runtime
                .event_cursor(RuntimeEventCursorRequest {
                    schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                    invocation,
                    run_id,
                    after_sequence: 0,
                    limit: 64,
                })
                .expect("running cursor");
            assert_eq!(page.state, RuntimeEventCursorState::Running);
        });
    })
    .await
    .expect("first Runtime thread");
}

async fn await_terminal(
    runtime: &EmbeddedRuntime,
    invocation: RuntimeInvocationContext,
    run_id: Uuid,
) -> RunStatus {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let page = runtime
                .event_cursor(RuntimeEventCursorRequest {
                    schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                    invocation,
                    run_id,
                    after_sequence: 0,
                    limit: 256,
                })
                .expect("event cursor");
            if let RuntimeEventCursorState::Terminal { status } = page.state {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovered Run never terminated")
}

fn profile(
    invocation: RuntimeInvocationContext,
    state_root: &Path,
    workspace_root: &Path,
    endpoint: String,
) -> RuntimeProfile {
    RuntimeProfile {
        invocation,
        config: config(
            state_root.to_path_buf(),
            workspace_root.canonicalize().expect("workspace"),
            endpoint,
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregate_recovery_scans_every_profile_and_is_idempotent() {
    let state_a = tempfile::tempdir().expect("state A");
    let workspace_a = tempfile::tempdir().expect("workspace A");
    let state_b = tempfile::tempdir().expect("state B");
    let workspace_b = tempfile::tempdir().expect("workspace B");
    let invocation_a = invocation(Uuid::now_v7());
    let invocation_b = invocation(Uuid::now_v7());
    let run_a = Uuid::now_v7();
    let run_b = Uuid::now_v7();
    let (endpoint_a, first_a, calls_a, server_a) = spawn_recoverable_provider("tenant-a").await;
    let (endpoint_b, first_b, calls_b, server_b) = spawn_recoverable_provider("tenant-b").await;
    let profile_a = profile(invocation_a, state_a.path(), workspace_a.path(), endpoint_a);
    let profile_b = profile(invocation_b, state_b.path(), workspace_b.path(), endpoint_b);

    tokio::join!(
        leave_orphaned_running_run(profile_a.clone(), run_a, first_a),
        leave_orphaned_running_run(profile_b.clone(), run_b, first_b),
    );

    let runtime = Arc::new(
        EmbeddedRuntime::new(limits(), vec![profile_a, profile_b]).expect("replacement Runtime"),
    );
    let (first_report, racing_report) = tokio::join!(
        runtime.recover_all_unfinished_detached(),
        runtime.recover_all_unfinished_detached(),
    );
    assert_eq!(first_report.scanned_profiles, 2);
    assert_eq!(racing_report.scanned_profiles, 2);
    assert_eq!(
        first_report.recovered_runs + racing_report.recovered_runs,
        2,
        "concurrent scans must divide the same recovery work exactly once"
    );
    assert!(
        first_report.failures.is_empty(),
        "{:#?}",
        first_report.failures
    );
    assert!(
        racing_report.failures.is_empty(),
        "{:#?}",
        racing_report.failures
    );
    assert_eq!(
        tokio::join!(
            await_terminal(&runtime, invocation_a, run_a),
            await_terminal(&runtime, invocation_b, run_b),
        ),
        (RunStatus::Succeeded, RunStatus::Succeeded)
    );
    server_a.await.expect("provider A");
    server_b.await.expect("provider B");
    assert_eq!(calls_a.load(Ordering::SeqCst), 2);
    assert_eq!(calls_b.load(Ordering::SeqCst), 2);

    let repeated = runtime.recover_all_unfinished_detached().await;
    assert_eq!(repeated.scanned_profiles, 2);
    assert_eq!(repeated.recovered_runs, 0);
    assert!(repeated.failures.is_empty(), "{:#?}", repeated.failures);
    assert_eq!(calls_a.load(Ordering::SeqCst), 2);
    assert_eq!(calls_b.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_corrupt_profile_cannot_block_another_tenants_recovery() {
    let healthy_state = tempfile::tempdir().expect("healthy state");
    let healthy_workspace = tempfile::tempdir().expect("healthy workspace");
    let corrupt_state = tempfile::tempdir().expect("corrupt state");
    let corrupt_workspace = tempfile::tempdir().expect("corrupt workspace");
    let healthy_invocation = invocation(Uuid::now_v7());
    let corrupt_invocation = invocation(Uuid::now_v7());
    let healthy_run = Uuid::now_v7();
    let (endpoint, first_seen, calls, server) = spawn_recoverable_provider("healthy").await;
    let healthy_profile = profile(
        healthy_invocation,
        healthy_state.path(),
        healthy_workspace.path(),
        endpoint,
    );
    leave_orphaned_running_run(healthy_profile.clone(), healthy_run, first_seen).await;

    let corrupt_run = Uuid::now_v7();
    let corrupt_run_dir = corrupt_state
        .path()
        .join("runs")
        .join(corrupt_run.to_string());
    std::fs::create_dir_all(&corrupt_run_dir).expect("corrupt Run directory");
    std::fs::write(corrupt_run_dir.join("run.json"), b"{not-json").expect("corrupt Run record");
    let corrupt_profile = profile(
        corrupt_invocation,
        corrupt_state.path(),
        corrupt_workspace.path(),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );

    let runtime = Arc::new(
        EmbeddedRuntime::new(limits(), vec![corrupt_profile, healthy_profile])
            .expect("replacement Runtime"),
    );
    let report = runtime.recover_all_unfinished_detached().await;
    assert_eq!(report.scanned_profiles, 2);
    assert_eq!(report.recovered_runs, 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].invocation, corrupt_invocation);
    assert_eq!(
        await_terminal(&runtime, healthy_invocation, healthy_run).await,
        RunStatus::Succeeded
    );
    server.await.expect("healthy provider");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let repeated = runtime.recover_all_unfinished_detached().await;
    assert_eq!(repeated.scanned_profiles, 2);
    assert_eq!(repeated.recovered_runs, 0);
    assert_eq!(repeated.failures.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_unreadable_run_directory_is_reported_instead_of_looking_empty() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            limits(),
            vec![profile(
                identity,
                state.path(),
                workspace.path(),
                "http://127.0.0.1:1/v1/chat/completions".into(),
            )],
        )
        .expect("Runtime"),
    );
    let runs = state.path().join("runs");
    if runs.is_dir() {
        std::fs::remove_dir_all(&runs).expect("remove empty runs directory");
    }
    std::fs::write(&runs, b"not a directory").expect("replace runs directory");

    assert!(
        LocalRuntimeHost::list_run_records(state.path()).is_err(),
        "the authoritative Run enumerator must not convert a scan failure into an empty list"
    );

    let report = runtime.recover_all_unfinished_detached().await;
    assert_eq!(report.scanned_profiles, 1);
    assert_eq!(report.recovered_runs, 0);
    assert_eq!(
        report.failures.len(),
        1,
        "storage failure must not look empty"
    );
    assert_eq!(report.failures[0].invocation, identity);
}
