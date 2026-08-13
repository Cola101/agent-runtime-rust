use agent_protocol::RuntimeInvocationContext;
use agent_runtime_host::admission::{RuntimeAdmissionController, RuntimeAdmissionLimits};
use std::sync::Arc;
use std::time::Duration;
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

async fn wait_until_queued(controller: &RuntimeAdmissionController, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if controller.snapshot().queued_runs == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admission request did not enter the bounded queue");
}

#[tokio::test]
async fn a_busy_tenant_cannot_take_the_next_slot_from_another_tenant() {
    let controller = Arc::new(
        RuntimeAdmissionController::new(RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 4,
            max_queued_runs_per_tenant: 2,
        })
        .expect("limits"),
    );
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let first = controller
        .acquire(invocation(tenant_a))
        .await
        .expect("first A Run");

    let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
    let a_controller = Arc::clone(&controller);
    let a_tx = order_tx.clone();
    let queued_a = tokio::spawn(async move {
        let permit = a_controller
            .acquire(invocation(tenant_a))
            .await
            .expect("queued A Run");
        a_tx.send("A").unwrap();
        permit
    });
    wait_until_queued(&controller, 1).await;

    let b_controller = Arc::clone(&controller);
    let queued_b = tokio::spawn(async move {
        let permit = b_controller
            .acquire(invocation(tenant_b))
            .await
            .expect("queued B Run");
        order_tx.send("B").unwrap();
        permit
    });
    wait_until_queued(&controller, 2).await;

    drop(first);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), order_rx.recv())
            .await
            .expect("one tenant must be admitted"),
        Some("B"),
        "round-robin admission must move away from the tenant that just ran"
    );

    let b_permit = queued_b.await.expect("B task");
    drop(b_permit);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), order_rx.recv())
            .await
            .expect("A must eventually be admitted"),
        Some("A")
    );
    drop(queued_a.await.expect("A task"));
}

#[tokio::test]
async fn admission_is_bounded_globally_and_per_tenant() {
    let controller = Arc::new(
        RuntimeAdmissionController::new(RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 2,
            max_queued_runs_per_tenant: 1,
        })
        .expect("limits"),
    );
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let active = controller
        .acquire(invocation(tenant_a))
        .await
        .expect("active Run");

    let queued_controller = Arc::clone(&controller);
    let queued = tokio::spawn(async move { queued_controller.acquire(invocation(tenant_a)).await });
    wait_until_queued(&controller, 1).await;

    let tenant_error = controller
        .acquire(invocation(tenant_a))
        .await
        .expect_err("tenant queue must be bounded");
    assert!(tenant_error.to_string().contains("tenant queue"));

    let other_controller = Arc::clone(&controller);
    let other = tokio::spawn(async move { other_controller.acquire(invocation(tenant_b)).await });
    wait_until_queued(&controller, 2).await;
    let global_error = controller
        .acquire(invocation(Uuid::now_v7()))
        .await
        .expect_err("global queue must be bounded");
    assert!(global_error.to_string().contains("global queue"));

    queued.abort();
    other.abort();
    drop(active);
}

#[tokio::test]
async fn cancelling_a_waiter_releases_queue_capacity_without_waiting_for_an_active_run() {
    let controller = Arc::new(
        RuntimeAdmissionController::new(RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        })
        .expect("limits"),
    );
    let active = controller
        .acquire(invocation(Uuid::now_v7()))
        .await
        .expect("active Run");
    let queued_controller = Arc::clone(&controller);
    let queued =
        tokio::spawn(async move { queued_controller.acquire(invocation(Uuid::now_v7())).await });
    wait_until_queued(&controller, 1).await;

    queued.abort();
    let _ = queued.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if controller.snapshot().queued_runs == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled waiter must not retain the bounded queue slot");

    assert_eq!(controller.snapshot().active_runs, 1);
    drop(active);
}

#[tokio::test]
async fn one_workspace_is_single_writer_without_blocking_another_workspace_of_the_tenant() {
    let controller = Arc::new(
        RuntimeAdmissionController::new(RuntimeAdmissionLimits {
            max_active_runs: 2,
            max_active_runs_per_tenant: 2,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 4,
            max_queued_runs_per_tenant: 4,
        })
        .expect("limits"),
    );
    let tenant = Uuid::now_v7();
    let workspace_a = invocation(tenant);
    let first = controller
        .acquire(workspace_a)
        .await
        .expect("first workspace owner");

    let same_controller = Arc::clone(&controller);
    let same_workspace = tokio::spawn(async move { same_controller.acquire(workspace_a).await });
    wait_until_queued(&controller, 1).await;

    let mut workspace_b = workspace_a;
    workspace_b.workspace_id = Uuid::now_v7();
    let second = controller
        .acquire(workspace_b)
        .await
        .expect("another workspace should use the free global slot");
    assert_eq!(controller.snapshot().active_runs, 2);
    assert_eq!(controller.snapshot().active_workspaces, 2);
    assert_eq!(controller.snapshot().queued_runs, 1);

    drop(first);
    let same = tokio::time::timeout(Duration::from_secs(1), same_workspace)
        .await
        .expect("same workspace should acquire after owner release")
        .expect("waiter task")
        .expect("same workspace permit");
    drop(same);
    drop(second);
}
