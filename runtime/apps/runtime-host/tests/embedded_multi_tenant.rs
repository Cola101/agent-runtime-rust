use agent_model_gateway::{Capability, DataClass, ProviderProtocol};
use agent_protocol::{
    RunBudget, RunStatus, RuntimeExecutionPolicySnapshot, RuntimeInvocationContext,
};
use agent_runtime_host::admission::RuntimeAdmissionLimits;
use agent_runtime_host::embedded::{
    EmbeddedRuntime, RUNTIME_EVENT_CURSOR_SCHEMA_VERSION, RuntimeEventCursorRequest,
    RuntimeEventCursorState, RuntimeEventStreamItem, RuntimeProfile,
};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalMcpServerConfig, LocalMcpTransportConfig,
    LocalModelRoutingConfig, LocalProviderConfig, LocalRuntimeConfig, LocalToolConsent,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
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

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Answer from the current invocation only.".into(),
        delegated_scopes: BTreeSet::new(),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig {
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
                response_timeout_ms: 5_000,
                stream_idle_timeout_ms: 5_000,
            }],
        },
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::Ask,
        budget: RunBudget {
            max_tokens: 1_000,
            max_cost_cents: 100,
            max_duration_seconds: 60,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

async fn spawn_provider(
    label: &'static str,
    requests: usize,
    mut first_release: Option<tokio::sync::oneshot::Receiver<()>>,
    order: tokio::sync::mpsc::UnboundedSender<&'static str>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("addr")
    );
    let task = tokio::spawn(async move {
        for index in 0..requests {
            let (mut socket, _) = listener.accept().await.expect("provider request");
            order.send(label).expect("order receiver");
            let mut request = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            if index == 0
                && let Some(release) = first_release.take()
            {
                let _ = release.await;
            }
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{label}\"}}}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("reply");
        }
    });
    (endpoint, task)
}

async fn wait_for_queue(runtime: &EmbeddedRuntime, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if runtime.admission_snapshot().queued_runs == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Run did not enter admission queue");
}

#[tokio::test]
async fn a_provider_selection_failure_is_a_durable_terminal_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let mut profile_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    // The candidate is valid configuration but cannot satisfy this Run's cost
    // policy. Selection therefore fails before any Provider request is made.
    profile_config
        .model_routing
        .max_cost_per_million_tokens_micros = 0;
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 1,
                max_queued_runs_per_tenant: 1,
            },
            vec![RuntimeProfile {
                invocation: identity,
                config: profile_config,
            }],
        )
        .expect("embedded Runtime"),
    );
    let run_id = Uuid::now_v7();

    runtime
        .execute_detached(identity, run_id, "fail before Provider egress".into())
        .await
        .expect("Run admission");

    let page = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match runtime.event_cursor(RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: identity,
                run_id,
                after_sequence: 0,
                limit: 64,
            }) {
                Ok(page)
                    if matches!(
                        page.state,
                        RuntimeEventCursorState::Terminal {
                            status: RunStatus::Failed
                        }
                    ) =>
                {
                    return page;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(error) => panic!("terminal Run became unreadable: {error}"),
            }
        }
    })
    .await
    .expect("provider selection failure never reached a terminal boundary");

    assert_eq!(
        page.events.last().map(|event| event.event_type.as_str()),
        Some("run.failed")
    );
    assert_eq!(
        page.events
            .iter()
            .filter(|event| event.event_type == "run.failed")
            .count(),
        1,
        "one Run must have exactly one terminal event"
    );

    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(
            state
                .path()
                .join("runs")
                .join(run_id.to_string())
                .join("events.jsonl"),
        )
        .expect("event log")
        .write_all(b"{\"event_id\":\"torn")
        .expect("inject crash tail");
    let after_crash = runtime
        .event_cursor(RuntimeEventCursorRequest {
            schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
            invocation: identity,
            run_id,
            after_sequence: 0,
            limit: 64,
        })
        .expect("an unterminated row is not committed");
    assert_eq!(after_crash.events, page.events);
    assert_eq!(after_crash.state, page.state);
}

#[tokio::test]
async fn a_session_directory_scan_failure_cannot_publish_a_fake_terminal_event() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let mut profile_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().expect("workspace"),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    profile_config
        .model_routing
        .max_cost_per_million_tokens_micros = 0;
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![RuntimeProfile {
            invocation: identity,
            config: profile_config,
        }],
    )
    .expect("Runtime");
    std::fs::write(state.path().join("sessions"), b"not a directory")
        .expect("break Session authority scan");
    let run_id = Uuid::now_v7();

    let result = runtime
        .execute(identity, run_id, "fail before Provider egress")
        .await;
    assert!(
        result.is_err(),
        "a terminal event cannot be published when Session ownership cannot be checked"
    );
    assert!(
        agent_runtime_host::LocalRuntimeHost::replay_events(state.path(), run_id, 0)
            .expect("committed prefix")
            .iter()
            .all(|event| !matches!(
                event.event_type.as_str(),
                "run.succeeded"
                    | "run.failed"
                    | "run.cancelled"
                    | "run.timed_out"
                    | "run.indeterminate"
            )),
        "storage uncertainty must not be converted into a terminal Run"
    );
}

#[tokio::test]
async fn a_live_subscription_continues_after_a_torn_tail_is_repaired() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
    let (endpoint, provider) = spawn_provider("resumed", 1, Some(release_rx), order_tx).await;
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 1,
                max_queued_runs_per_tenant: 1,
            },
            vec![RuntimeProfile {
                invocation: identity,
                config: config(
                    state.path().to_path_buf(),
                    workspace.path().canonicalize().expect("workspace"),
                    endpoint,
                ),
            }],
        )
        .expect("Runtime"),
    );
    let run_id = Uuid::now_v7();
    runtime
        .execute_detached(identity, run_id, "wait for the Provider".into())
        .await
        .expect("Run admission");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), order_rx.recv())
            .await
            .expect("Provider request was never observed"),
        Some("resumed")
    );

    let event_log = state
        .path()
        .join("runs")
        .join(run_id.to_string())
        .join("events.jsonl");
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&event_log)
        .expect("event log")
        .write_all(b"{\"event_id\":\"torn")
        .expect("inject crash tail");

    let mut subscription = runtime
        .subscribe_events(identity, run_id, 0, 16)
        .expect("subscription accepts an uncommitted tail");
    release_tx.send(()).expect("release Provider");
    let mut sequences = Vec::new();
    let boundary = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match subscription.recv().await.expect("subscription item") {
                Ok(RuntimeEventStreamItem::Event { event, .. }) => {
                    sequences.push(event.sequence);
                }
                Ok(RuntimeEventStreamItem::Boundary { state, .. }) => return state,
                Err(error) => panic!("subscription failed after tail repair: {error}"),
            }
        }
    })
    .await
    .expect("subscription never reached a boundary");
    assert_eq!(
        boundary,
        RuntimeEventCursorState::Terminal {
            status: RunStatus::Succeeded
        }
    );
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
    provider.await.expect("Provider");
}

#[tokio::test]
async fn an_unavailable_required_mcp_server_is_a_durable_terminal_run() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let mut profile_config = config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    profile_config
        .runtime_policy
        .mcp_discovery
        .max_attempts_per_server = 1;
    profile_config
        .runtime_policy
        .mcp_discovery
        .initial_retry_backoff_ms = 0;
    profile_config
        .delegated_scopes
        .insert("tool:mcp:required-local".into());
    profile_config.mcp_servers = vec![LocalMcpServerConfig {
        server_id: Uuid::now_v7(),
        name: "required-local".into(),
        transport: LocalMcpTransportConfig::StreamableHttp {
            endpoint: "http://127.0.0.1:1/mcp".into(),
        },
        tool_names: BTreeSet::from(["lookup".into()]),
        tool_effect_overrides: BTreeMap::new(),
        required: true,
    }];
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 1,
                max_queued_runs_per_tenant: 1,
            },
            vec![RuntimeProfile {
                invocation: identity,
                config: profile_config,
            }],
        )
        .expect("embedded Runtime"),
    );
    let run_id = Uuid::now_v7();

    runtime
        .execute_detached(identity, run_id, "require the unavailable Tool".into())
        .await
        .expect("Run admission");

    let page = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match runtime.event_cursor(RuntimeEventCursorRequest {
                schema_version: RUNTIME_EVENT_CURSOR_SCHEMA_VERSION,
                invocation: identity,
                run_id,
                after_sequence: 0,
                limit: 64,
            }) {
                Ok(page)
                    if matches!(
                        page.state,
                        RuntimeEventCursorState::Terminal {
                            status: RunStatus::Failed
                        }
                    ) =>
                {
                    return page;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(error) => panic!(
                    "required MCP failure became unreadable: {error}; record={:?}; events={:?}",
                    agent_runtime_host::LocalRuntimeHost::read_run_record(state.path(), run_id),
                    agent_runtime_host::LocalRuntimeHost::replay_events(state.path(), run_id, 0)
                ),
            }
        }
    })
    .await
    .expect("required MCP failure never reached a terminal boundary");

    assert_eq!(
        page.events.last().map(|event| event.event_type.as_str()),
        Some("run.failed")
    );
    assert_eq!(
        page.events.last().unwrap().payload["kind"],
        "required_mcp_unavailable"
    );
    assert_eq!(
        page.events.last().unwrap().payload["servers"][0],
        "required-local"
    );
    assert_eq!(
        page.events
            .iter()
            .filter(|event| event.event_type == "run.failed")
            .count(),
        1
    );
    assert!(
        page.events
            .iter()
            .all(|event| event.event_type != "run.started"),
        "MCP discovery failed before model execution started"
    );
}

#[tokio::test]
async fn registered_tenants_share_one_runtime_without_losing_fairness_or_identity() {
    let state_a = tempfile::tempdir().expect("state A");
    let state_b = tempfile::tempdir().expect("state B");
    let workspace_a = tempfile::tempdir().expect("workspace A");
    let workspace_b = tempfile::tempdir().expect("workspace B");
    let tenant_a = invocation(Uuid::now_v7());
    let tenant_b = invocation(Uuid::now_v7());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
    let (endpoint_a, provider_a) = spawn_provider("A", 2, Some(release_rx), order_tx.clone()).await;
    let (endpoint_b, provider_b) = spawn_provider("B", 1, None, order_tx).await;
    let runtime = Arc::new(
        EmbeddedRuntime::new(
            RuntimeAdmissionLimits {
                max_active_runs: 1,
                max_active_runs_per_tenant: 1,
                max_active_runs_per_workspace: 1,
                max_queued_runs: 8,
                max_queued_runs_per_tenant: 4,
            },
            vec![
                RuntimeProfile {
                    invocation: tenant_a,
                    config: config(
                        state_a.path().to_path_buf(),
                        workspace_a.path().canonicalize().unwrap(),
                        endpoint_a,
                    ),
                },
                RuntimeProfile {
                    invocation: tenant_b,
                    config: config(
                        state_b.path().to_path_buf(),
                        workspace_b.path().canonicalize().unwrap(),
                        endpoint_b,
                    ),
                },
            ],
        )
        .expect("embedded Runtime"),
    );

    let first_runtime = Arc::clone(&runtime);
    let first = tokio::spawn(async move {
        first_runtime
            .execute(tenant_a, Uuid::now_v7(), "first A")
            .await
    });
    assert_eq!(order_rx.recv().await, Some("A"));

    let second_runtime = Arc::clone(&runtime);
    let second_a = tokio::spawn(async move {
        second_runtime
            .execute(tenant_a, Uuid::now_v7(), "second A")
            .await
    });
    wait_for_queue(&runtime, 1).await;
    let b_runtime = Arc::clone(&runtime);
    let run_b =
        tokio::spawn(async move { b_runtime.execute(tenant_b, Uuid::now_v7(), "one B").await });
    wait_for_queue(&runtime, 2).await;

    release_tx.send(()).expect("release first A");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), order_rx.recv())
            .await
            .expect("next provider request"),
        Some("B")
    );
    assert_eq!(run_b.await.unwrap().unwrap().output, "B");
    assert_eq!(order_rx.recv().await, Some("A"));

    for outcome in [
        first.await.unwrap().unwrap(),
        second_a.await.unwrap().unwrap(),
    ] {
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.output, "A");
    }
    provider_a.await.unwrap();
    provider_b.await.unwrap();
}

/// The production break this catches is accepting a control-plane Workspace
/// owner epoch at the Edge boundary but silently replacing it with the local
/// default. Recovery would then compare two different fencing histories.
#[tokio::test]
async fn embedded_execution_preserves_the_supplied_workspace_owner_epoch() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let identity = invocation(Uuid::now_v7());
    let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
    let (endpoint, provider) = spawn_provider("edge", 1, None, order_tx).await;
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![RuntimeProfile {
            invocation: identity,
            config: config(
                state.path().to_path_buf(),
                workspace.path().canonicalize().unwrap(),
                endpoint,
            ),
        }],
    )
    .expect("embedded Runtime");
    let run_id = Uuid::now_v7();

    let outcome = runtime
        .execute_at_epoch(identity, run_id, "edge task", 17)
        .await
        .expect("edge execution");
    assert_eq!(order_rx.recv().await, Some("edge"));
    let checkpoint =
        agent_runtime_host::LocalRuntimeHost::load_checkpoint(outcome.checkpoint_path.as_path())
            .expect("checkpoint");
    let checkpoint_state: serde_json::Value =
        serde_json::from_slice(&checkpoint.state).expect("checkpoint state");
    assert_eq!(checkpoint_state["owner_epoch"], 17);
    let events = agent_runtime_host::LocalRuntimeHost::replay_events(state.path(), run_id, 0)
        .expect("durable events");
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| {
        event.schema_version == 1
            && event.session_id == run_id
            && !event.attempt_id.is_nil()
            && !event.trace_id.is_nil()
            && event.digest.len() == 64
    }));
    provider.await.unwrap();
}

#[tokio::test]
async fn an_unregistered_workspace_identity_is_rejected_before_host_or_provider_start() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let registered = invocation(Uuid::now_v7());
    let runtime = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![RuntimeProfile {
            invocation: registered,
            config: config(
                state.path().to_path_buf(),
                workspace.path().canonicalize().unwrap(),
                "http://127.0.0.1:1/v1/chat/completions".into(),
            ),
        }],
    )
    .expect("embedded Runtime");
    let mut forged = registered;
    forged.workspace_id = Uuid::now_v7();

    let error = runtime
        .execute(forged, Uuid::now_v7(), "forged workspace")
        .await
        .expect_err("unregistered identity must fail before egress");
    assert!(error.to_string().contains("not registered"));
    assert_eq!(runtime.admission_snapshot().active_runs, 0);
}

/// The production break this catches is treating an immutable AgentVersion as
/// the owner of persistent Workspace storage. A Workspace must be reusable by
/// multiple registered AgentVersions inside the same tenant/application
/// boundary.
#[test]
fn one_workspace_can_register_multiple_agent_versions_against_the_same_roots() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let first = invocation(Uuid::now_v7());
    let mut second = first;
    second.workload_identity_id = Uuid::now_v7();
    second.agent_version_id = Uuid::now_v7();
    second.model_policy_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();

    EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    state.path().to_path_buf(),
                    workspace_root.clone(),
                    "http://127.0.0.1:1/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    state.path().to_path_buf(),
                    workspace_root,
                    "http://127.0.0.1:2/v1/chat/completions".into(),
                ),
            },
        ],
    )
    .expect("one Workspace may have multiple immutable AgentVersion profiles");
}

/// The production break this catches is accepting two different Workspace
/// identities that point at the same persistent roots, bypassing the Runtime's
/// tenant/application/Workspace storage boundary.
#[test]
fn different_workspace_identities_cannot_share_persistent_roots() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let first = invocation(Uuid::now_v7());
    let mut second = first;
    second.workspace_id = Uuid::now_v7();
    second.agent_version_id = Uuid::now_v7();
    let workspace_root = workspace.path().canonicalize().unwrap();

    let result = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    state.path().to_path_buf(),
                    workspace_root.clone(),
                    "http://127.0.0.1:1/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    state.path().to_path_buf(),
                    workspace_root,
                    "http://127.0.0.1:2/v1/chat/completions".into(),
                ),
            },
        ],
    );
    let error = match result {
        Ok(_) => panic!("different Workspace identities must not share persistent roots"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("owned by another Workspace"));
}

/// The production break this catches is registering the same state directory
/// under two lexical paths (for example `state` and `state/alias/..`) so two
/// tenant Workspaces bypass the persistent-root ownership map.
#[test]
fn different_workspace_identities_cannot_share_a_state_root_alias() {
    let state = tempfile::tempdir().expect("state");
    std::fs::create_dir(state.path().join("alias")).expect("alias component");
    let workspace_a = tempfile::tempdir().expect("workspace A");
    let workspace_b = tempfile::tempdir().expect("workspace B");
    let first = invocation(Uuid::now_v7());
    let mut second = first;
    second.workspace_id = Uuid::now_v7();
    second.agent_version_id = Uuid::now_v7();

    let result = EmbeddedRuntime::new(
        RuntimeAdmissionLimits {
            max_active_runs: 1,
            max_active_runs_per_tenant: 1,
            max_active_runs_per_workspace: 1,
            max_queued_runs: 1,
            max_queued_runs_per_tenant: 1,
        },
        vec![
            RuntimeProfile {
                invocation: first,
                config: config(
                    state.path().to_path_buf(),
                    workspace_a.path().canonicalize().unwrap(),
                    "http://127.0.0.1:1/v1/chat/completions".into(),
                ),
            },
            RuntimeProfile {
                invocation: second,
                config: config(
                    state.path().join("alias").join(".."),
                    workspace_b.path().canonicalize().unwrap(),
                    "http://127.0.0.1:2/v1/chat/completions".into(),
                ),
            },
        ],
    );

    assert!(result.is_err());
}

/// The production break this catches is treating any event-log I/O failure as
/// an empty log. Edge reconciliation could then publish a successful receipt
/// without the Runtime events that are supposed to prove it.
#[test]
fn durable_event_log_read_errors_fail_closed() {
    let state = tempfile::tempdir().expect("state");
    let run_id = Uuid::now_v7();
    std::fs::create_dir_all(
        state
            .path()
            .join("runs")
            .join(run_id.to_string())
            .join("events.jsonl"),
    )
    .expect("invalid event-log directory");

    assert!(agent_runtime_host::LocalRuntimeHost::replay_events(state.path(), run_id, 0).is_err());
}

#[test]
fn committed_event_log_corruption_is_never_tail_repaired() {
    let state = tempfile::tempdir().expect("state");
    let run_id = Uuid::now_v7();
    let run_dir = state.path().join("runs").join(run_id.to_string());
    std::fs::create_dir_all(&run_dir).expect("Run directory");
    std::fs::write(run_dir.join("events.jsonl"), b"{not-json}\n").expect("corrupt committed row");

    assert!(agent_runtime_host::LocalRuntimeHost::replay_events(state.path(), run_id, 0).is_err());
}
