//! The Runtime as something that starts, reports itself, and stops.
//!
//! The property that matters to a desktop application is that quitting is not
//! cancelling. Everything here is about the machine around that: one pass
//! through the lifecycle, several callers told the same thing, and a report
//! that survives the process it was produced for.

use agent_protocol::{RunBudget, RuntimeInvocationContext};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::controller::{RuntimeController, RuntimeLifecycle};
use agent_runtime_host::embedded::{EmbeddedRuntime, RuntimeProfile};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRuntimeConfig, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

fn invocation() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: 1,
        tenant_id: uuid::Uuid::now_v7(),
        application_id: uuid::Uuid::now_v7(),
        workload_identity_id: uuid::Uuid::now_v7(),
        workspace_id: uuid::Uuid::now_v7(),
        agent_version_id: uuid::Uuid::now_v7(),
        model_policy_id: uuid::Uuid::now_v7(),
    }
}

fn runtime(state_root: &std::path::Path, workspace_root: &std::path::Path) -> Arc<EmbeddedRuntime> {
    runtime_with_provider(
        state_root,
        workspace_root,
        "http://127.0.0.1:1/v1/chat/completions",
    )
}

fn runtime_with_provider(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    endpoint: &str,
) -> Arc<EmbeddedRuntime> {
    runtime_for(state_root, workspace_root, endpoint, invocation())
}

fn local_config(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    endpoint: &str,
) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root: state_root.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
        agent_instructions: "Answer briefly.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            endpoint,
            "test-model",
            "test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_000,
            max_cost_cents: 100,
            max_duration_seconds: 60,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    }
}

fn runtime_for(
    state_root: &std::path::Path,
    workspace_root: &std::path::Path,
    endpoint: &str,
    profile: RuntimeInvocationContext,
) -> Arc<EmbeddedRuntime> {
    Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 4,
                max_active_runs_per_tenant: 4,
                max_active_runs_per_workspace: 2,
                max_queued_runs: 8,
                max_queued_runs_per_tenant: 8,
            },
            vec![RuntimeProfile {
                invocation: profile,
                config: LocalRuntimeConfig {
                    state_root: state_root.to_path_buf(),
                    workspace_root: workspace_root.to_path_buf(),
                    agent_instructions: "Answer briefly.".into(),
                    delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
                    subagent_roles: Vec::new(),
                    model_routing: LocalModelRoutingConfig::single_openai_compatible(
                        endpoint,
                        "test-model",
                        "test-key",
                    ),
                    mcp_servers: Vec::new(),
                    mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
                    trusted_workspace_tool: None,
                    process_session: None,
                    consent: LocalToolConsent::Ask,
                    budget: RunBudget {
                        max_tokens: 1_000,
                        max_cost_cents: 100,
                        max_duration_seconds: 60,
                    },
                    runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
                },
            }],
        )
        .expect("runtime"),
    )
}

/// One pass, in order, and no way back to the beginning.
#[tokio::test(flavor = "multi_thread")]
async fn the_lifecycle_runs_once_and_does_not_restart() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let controller = RuntimeController::with_drain_deadline(
        runtime(state.path(), workspace.path()),
        // Deliberately tiny. A deadline a test has to sit through is a deadline
        // that will one day fail for having sat through it on a busy machine.
        Duration::from_millis(50),
    );

    assert_eq!(controller.lifecycle().await, RuntimeLifecycle::Created);
    controller.start().await.expect("start");
    assert_eq!(controller.lifecycle().await, RuntimeLifecycle::Ready);

    // Starting an already-open Runtime is not an error; it is a caller that
    // does not know it was beaten to it.
    controller
        .start()
        .await
        .expect("start is idempotent once Ready");

    controller.shutdown().await;
    assert_eq!(controller.lifecycle().await, RuntimeLifecycle::Stopped);

    // A stopped instance holds one pass's state root lease, recovered Runs and
    // owner epochs. Restarting it would hand a second pass the first one's
    // assumptions.
    controller
        .start()
        .await
        .expect_err("a stopped Runtime must not restart");
}

/// Several callers, one transition, one answer.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_callers_are_told_the_same_thing() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let controller = RuntimeController::with_drain_deadline(
        runtime(state.path(), workspace.path()),
        Duration::from_millis(50),
    );

    let starts: Vec<_> = (0..8)
        .map(|_| {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move { controller.start().await })
        })
        .collect();
    for start in starts {
        start.await.expect("join").expect("every waiter starts");
    }
    assert_eq!(controller.lifecycle().await, RuntimeLifecycle::Ready);

    let shutdowns: Vec<_> = (0..8)
        .map(|_| {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move { controller.shutdown().await })
        })
        .collect();
    let mut reports = Vec::new();
    for shutdown in shutdowns {
        reports.push(shutdown.await.expect("join"));
    }
    // "How did the shutdown go" has one answer per instance. Two callers being
    // told different things would mean two drains happened.
    let first = reports.first().expect("a report").clone();
    assert!(
        reports.iter().all(|report| *report == first),
        "concurrent shutdowns produced differing reports: {reports:?}"
    );
    assert_eq!(controller.lifecycle().await, RuntimeLifecycle::Stopped);
}

/// The report outlives the call that produced it.
///
/// For a desktop application the caller of `shutdown` is a process on its way
/// out, so the counts it returns are read by nobody. They are exactly what the
/// next start should be able to say, and they are handed over once.
#[tokio::test(flavor = "multi_thread")]
async fn the_shutdown_report_is_handed_to_the_next_look_exactly_once() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let controller = RuntimeController::with_drain_deadline(
        runtime(state.path(), workspace.path()),
        Duration::from_millis(50),
    );

    controller.start().await.expect("start");
    assert!(
        controller.snapshot().await.previous_shutdown.is_none(),
        "a Runtime that has not shut down has nothing to hand over"
    );

    let returned = controller.shutdown().await;
    let handed = controller
        .snapshot()
        .await
        .previous_shutdown
        .expect("the shutdown is handed over");
    assert_eq!(handed, returned);

    // Once. Leaving it in place would make every later look report a shutdown
    // that has already been accounted for.
    assert!(controller.snapshot().await.previous_shutdown.is_none());
}

/// Recovery is visible while it is happening, not only once it is done.
#[tokio::test(flavor = "multi_thread")]
async fn a_started_runtime_reports_its_recovery_as_complete() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let controller = RuntimeController::with_drain_deadline(
        runtime(state.path(), workspace.path()),
        Duration::from_millis(50),
    );

    let before = controller.snapshot().await;
    assert_eq!(before.lifecycle, RuntimeLifecycle::Created);
    assert_eq!(before.recovery.total_profiles, 0);

    controller.start().await.expect("start");
    let after = controller.snapshot().await;
    assert_eq!(after.lifecycle, RuntimeLifecycle::Ready);
    assert_eq!(after.recovery.total_profiles, 1);
    assert_eq!(
        after.recovery.completed_profiles,
        after.recovery.total_profiles
    );
    assert!(
        after.recovery_failures.is_empty(),
        "an empty state root has nothing to fail at"
    );
}

/// Quitting is not cancelling.
///
/// This is the property the whole batch exists for. A Run stopped because the
/// Runtime is going away was not cancelled by anybody, and recording it as a
/// cancellation would tell the person who quit their application that they
/// cancelled work they never touched -- and would leave an operator Cancel
/// receipt in the audit trail attributing a decision nobody made.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_runtime_is_not_cancelling_anybody_s_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    // A Provider that accepts the connection and never answers, so the Turn is
    // genuinely in flight when the deadline arrives. Nothing here waits on a
    // duration: the Run is held open on purpose.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let invocation = invocation();
    let runtime = runtime_for(state.path(), workspace.path(), &endpoint, invocation);
    let controller =
        RuntimeController::with_drain_deadline(Arc::clone(&runtime), Duration::from_millis(50));
    controller.start().await.expect("start");

    let run_id = uuid::Uuid::now_v7();
    runtime
        .execute_detached(
            invocation,
            run_id,
            "a Turn that will still be running".into(),
        )
        .await
        .expect("accepted");
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.runtime_snapshot().active_execution_owners == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Run never became active");

    let report = controller.shutdown().await;
    assert!(
        report.deadline_reached,
        "the Run was supposed to outlast the drain"
    );
    assert!(report.stopped_at_deadline >= 1);

    // The durable log must not claim anybody cancelled this.
    let events = runtime
        .event_cursor(agent_runtime_host::embedded::RuntimeEventCursorRequest {
            schema_version: agent_runtime_host::embedded::RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation,
            run_id,
            after_sequence: 0,
            limit: 256,
        })
        .expect("cursor");
    assert!(
        !events
            .events
            .iter()
            .any(|event| event.event_type == "run.cancelled"),
        "stopping the Runtime published a cancellation: {:?}",
        events
            .events
            .iter()
            .map(|event| event.event_type.clone())
            .collect::<Vec<_>>()
    );

    // And no operator Cancel receipt was written for a decision nobody made.
    let receipts = runtime
        .list_control_receipts(invocation, run_id)
        .expect("receipts");
    assert!(
        receipts.is_empty(),
        "stopping the Runtime wrote a control receipt: {receipts:?}"
    );
}

/// Closing admission releases the queue instead of leaving it to wait.
///
/// A request that never got a slot has taken nothing and left nothing -- which
/// is precisely what lets the accept path put admission before its first
/// durable write. So a refusal here must leave no Run, no Session Turn and no
/// control receipt behind, and the caller must be told, not left holding a
/// future against a Runtime that is going away.
#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_releases_the_queue_and_leaves_nothing_behind() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    // A Provider that accepts and never answers, so the admitted Run holds its
    // only slot for the whole test rather than for a hopeful interval.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let invocation = invocation();
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 4,
                max_queued_runs_per_tenant: 4,
            },
            vec![RuntimeProfile {
                invocation,
                config: local_config(state.path(), workspace.path(), &endpoint),
            }],
        )
        .expect("runtime"),
    );
    let controller =
        RuntimeController::with_drain_deadline(Arc::clone(&runtime), Duration::from_millis(50));
    controller.start().await.expect("start");

    // One Run takes the only slot.
    let holder = uuid::Uuid::now_v7();
    runtime
        .execute_detached(invocation, holder, "hold the only slot".into())
        .await
        .expect("admitted");
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.runtime_snapshot().active_execution_owners == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Run never became active");

    // A second one queues. It parks inside admission, so it is spawned; the
    // queue depth is read from the snapshot rather than assumed.
    let queued_run = uuid::Uuid::now_v7();
    let queueing = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move {
            runtime
                .execute_detached(invocation, queued_run, "wait for a slot".into())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.runtime_snapshot().admission.queued_runs == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second Run never reached the queue");

    let report = controller.shutdown().await;
    assert_eq!(
        report.released_from_queue, 1,
        "the queued Run was left waiting on a Runtime that is leaving"
    );

    // Woken, not left hanging.
    let released = tokio::time::timeout(Duration::from_secs(20), queueing)
        .await
        .expect("the queued caller was never woken")
        .expect("join");
    released.expect_err("a released caller must be told, not admitted");

    // And it left nothing behind: no Run record for work that never started.
    assert!(
        runtime
            .read_run_record(invocation, queued_run)
            .expect("record lookup")
            .is_none(),
        "a request refused at admission created a Run anyway"
    );
}
