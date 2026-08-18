//! The owner surface on the local socket.
//!
//! Two questions get confused if they share one namespace. A workload asks the
//! Runtime to do work; whoever owns the state root asks it what it holds, and
//! eventually asks it to stop. The socket is created `0o600`, so connecting at
//! all is already the owner credential -- what the split buys is that neither
//! surface can be reached through the other, and that a client reading replies
//! never has to guess which kind it got.

use agent_protocol::RunBudget;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, OwnerRequest, OwnerResponse, OwnerRunState,
    default_socket_path,
};
use agent_runtime_host::{
    LocalModelRoutingConfig, LocalRuntimeConfig, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};

const REPLY: &str = "the owner surface answered";

async fn spawn_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0_u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            let body = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{REPLY}\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                 data: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://127.0.0.1:{port}/v1/chat/completions")
}

fn config(state_root: PathBuf, workspace_root: PathBuf, endpoint: String) -> LocalRuntimeConfig {
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Answer briefly.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing: LocalModelRoutingConfig::single_openai_compatible(
            endpoint,
            "local-test-model",
            "local-test-key",
        ),
        mcp_servers: Vec::new(),
        mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: None,
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    }
}

/// A daemon that can actually be stopped.
///
/// The state root is held by an `flock` inside the `EmbeddedRuntime`, so a
/// replacement over the same directory requires the previous one to be gone --
/// not merely for its socket path to have been forgotten.
struct Daemon {
    socket: PathBuf,
    daemon: Option<std::sync::Arc<LocalRuntimeDaemon>>,
    runtime: Option<std::sync::Arc<agent_runtime_host::embedded::EmbeddedRuntime>>,
    serving: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(config: LocalRuntimeConfig) -> Self {
        let socket = default_socket_path(&config.state_root);
        let listener = LocalRuntimeDaemon::bind(&socket)
            .await
            .expect("bind socket");
        let daemon = LocalRuntimeDaemon::new(config);
        let runtime = daemon.runtime();
        let serving = tokio::spawn(std::sync::Arc::clone(&daemon).serve(listener));
        Self {
            socket,
            daemon: Some(daemon),
            runtime: Some(runtime),
            serving,
        }
    }

    /// Waits for the reference count rather than for a plausible interval: it
    /// converges the moment the last holder goes, and the bound only turns a
    /// hang into a readable failure.
    async fn stop(mut self) {
        self.serving.abort();
        let _ = (&mut self.serving).await;
        drop(self.daemon.take());
        let runtime = self.runtime.take().expect("runtime handle");
        tokio::time::timeout(Duration::from_secs(20), async {
            while std::sync::Arc::strong_count(&runtime) > 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the daemon never released its state root");
        drop(runtime);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Sends one raw line and reads one raw line, so a test can address either
/// namespace -- including with requests no typed client would construct.
async fn round_trip(socket: &Path, line: &str) -> String {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    writer.flush().await.expect("flush");
    BufReader::new(reader)
        .lines()
        .next_line()
        .await
        .expect("read")
        .expect("a reply")
}

async fn submit(socket: &Path, input: &str) -> uuid::Uuid {
    let line = serde_json::to_string(&LocalRequest::Submit {
        input: input.to_owned(),
    })
    .expect("encode");
    let reply: LocalResponse =
        serde_json::from_str(&round_trip(socket, &line).await).expect("decode");
    match reply {
        LocalResponse::Accepted { run_id } => run_id,
        other => panic!("expected acceptance, got {other:?}"),
    }
}

async fn list_runs(socket: &Path) -> Vec<agent_runtime_host::ipc::OwnerRunSummary> {
    let line = serde_json::to_string(&serde_json::json!({
        "scope": "owner",
        "type": "list_runs",
    }))
    .expect("encode");
    let reply: OwnerResponse =
        serde_json::from_str(&round_trip(socket, &line).await).expect("decode");
    match reply {
        OwnerResponse::Runs { runs, .. } => runs,
        other => panic!("expected a Run list, got {other:?}"),
    }
}

/// Neither namespace is reachable through the other, over the real socket.
#[tokio::test]
async fn the_owner_and_workload_namespaces_do_not_reach_each_other() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        spawn_provider().await,
    ))
    .await;
    let socket = host.socket.clone();

    // A workload operation named under the owner scope is refused, not run.
    let refused = round_trip(&socket, r#"{"scope":"owner","type":"list"}"#).await;
    let refused: LocalResponse = serde_json::from_str(&refused).expect("decode");
    assert!(
        matches!(refused, LocalResponse::Error { .. }),
        "a workload operation reached the owner scope: {refused:?}"
    );

    // And an owner operation is not reachable by leaving the scope off, which
    // is the direction that matters: every existing client leaves it off.
    let refused = round_trip(&socket, r#"{"type":"list_runs"}"#).await;
    let refused: LocalResponse = serde_json::from_str(&refused).expect("decode");
    assert!(
        matches!(refused, LocalResponse::Error { .. }),
        "an owner operation was reachable without naming the owner scope: {refused:?}"
    );

    // A scope nobody defined is refused rather than guessed at.
    let refused = round_trip(&socket, r#"{"scope":"admin","type":"list"}"#).await;
    let refused: LocalResponse = serde_json::from_str(&refused).expect("decode");
    assert!(matches!(refused, LocalResponse::Error { .. }));
}

/// The durable list survives a restart; the in-memory one does not, and that
/// difference is declared rather than discovered.
#[tokio::test]
async fn the_owner_list_reads_disk_where_the_workload_list_reads_memory() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let endpoint = spawn_provider().await;
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        endpoint.clone(),
    ))
    .await;
    let socket = host.socket.clone();

    let asked = "what does this state root hold?";
    let run_id = submit(&socket, asked).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let runs = list_runs(&socket).await;
            if runs
                .iter()
                .any(|run| run.run_id == run_id && !matches!(run.state, OwnerRunState::Running))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Run never reached a durable non-running state");

    let runs = list_runs(&socket).await;
    let run = runs
        .iter()
        .find(|run| run.run_id == run_id)
        .expect("the Run is on disk");
    // The input is on the durable record, so a client that did not submit this
    // Run can still say what it was asked to do.
    assert_eq!(run.input, asked);

    // A replacement daemon over the same state root: the workload list is
    // empty because it is this host's own order, and the owner list is not
    // because the Runs are still there.
    host.stop().await;
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        endpoint,
    ))
    .await;
    let socket = host.socket.clone();

    let line = serde_json::to_string(&LocalRequest::List).expect("encode");
    let reply: LocalResponse =
        serde_json::from_str(&round_trip(&socket, &line).await).expect("decode");
    let LocalResponse::Runs { run_ids } = reply else {
        panic!("expected the workload list");
    };
    assert!(
        run_ids.is_empty(),
        "the workload list is this host's own order and a new host has none"
    );

    let runs = list_runs(&socket).await;
    assert!(
        runs.iter().any(|run| run.run_id == run_id),
        "the owner list must still hold what the state root holds"
    );
}

/// Paging is bounded and its cursor is checked, not trusted.
#[tokio::test]
async fn the_owner_list_is_bounded_and_refuses_an_unknown_cursor() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        spawn_provider().await,
    ))
    .await;
    let socket = host.socket.clone();

    for over in ["0", "257"] {
        let line = format!(r#"{{"scope":"owner","type":"list_runs","limit":{over}}}"#);
        let reply: OwnerResponse =
            serde_json::from_str(&round_trip(&socket, &line).await).expect("decode");
        assert!(
            matches!(reply, OwnerResponse::Error { .. }),
            "a page limit outside the published range must be refused, not clamped"
        );
    }

    let line = format!(
        r#"{{"scope":"owner","type":"list_runs","after_run_id":"{}"}}"#,
        uuid::Uuid::now_v7()
    );
    let reply: OwnerResponse =
        serde_json::from_str(&round_trip(&socket, &line).await).expect("decode");
    assert!(
        matches!(reply, OwnerResponse::Error { .. }),
        "a cursor naming no known Run must be refused rather than silently restarting the page"
    );
}

async fn owner(socket: &Path, request: serde_json::Value) -> OwnerResponse {
    let line = serde_json::to_string(&request).expect("encode");
    serde_json::from_str(&round_trip(socket, &line).await).expect("decode")
}

fn head_of(response: OwnerResponse) -> agent_runtime_host::LocalSessionHead {
    match response {
        OwnerResponse::SessionHead { head } => *head,
        other => panic!("expected a Session head, got {other:?}"),
    }
}

/// Waits for the branch to have no Turn in flight, from what the owner surface
/// itself reports rather than from an interval.
async fn settled(
    socket: &Path,
    session_id: uuid::Uuid,
    branch_id: uuid::Uuid,
) -> agent_runtime_host::LocalSessionHead {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let head = head_of(
                owner(
                    socket,
                    serde_json::json!({
                        "scope": "owner",
                        "type": "session_read",
                        "session_id": session_id,
                        "branch_id": branch_id,
                    }),
                )
                .await,
            );
            if head.active_run_id.is_none() {
                return head;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the branch never settled")
}

/// The whole Session surface, over the socket, by a caller that supplies no
/// identity of any kind.
///
/// This is the batch's gate. The Session contract was hardened onto
/// `RuntimeClient` and gRPC while the desktop client speaks this socket, whose
/// requests contained no Session operation at all -- finished, and unreachable.
/// A caller here names only what it wants done: the daemon owns exactly one
/// state root and one identity, so asking a client to supply that identity
/// would only invite it to supply the wrong one.
#[tokio::test]
async fn the_owner_socket_carries_the_whole_session_chain() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        spawn_provider().await,
    ))
    .await;
    let socket = host.socket.clone();

    let session_id = uuid::Uuid::now_v7();
    let branch_id = uuid::Uuid::now_v7();

    // --- Start -----------------------------------------------------------
    let started = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_start",
            "session_id": session_id, "branch_id": branch_id,
            "run_id": uuid::Uuid::now_v7(), "input": "the first turn over the socket",
        }),
    )
    .await;
    assert!(
        matches!(started, OwnerResponse::SessionTurn { .. }),
        "expected a Turn receipt, got {started:?}"
    );
    let head = settled(&socket, session_id, branch_id).await;
    assert_eq!(head.turn_count, 1);

    // --- Continue ---------------------------------------------------------
    let continued = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_continue",
            "session_id": session_id, "branch_id": branch_id,
            "generation": head.generation, "run_id": uuid::Uuid::now_v7(),
            "input": "the second turn over the socket",
        }),
    )
    .await;
    assert!(matches!(continued, OwnerResponse::SessionTurn { .. }));
    let head = settled(&socket, session_id, branch_id).await;
    assert_eq!(head.turn_count, 2);

    // --- Fork -------------------------------------------------------------
    let fork_branch_id = uuid::Uuid::now_v7();
    let forked = head_of(
        owner(
            &socket,
            serde_json::json!({
                "scope": "owner", "type": "session_fork",
                "session_id": session_id, "source_branch_id": branch_id,
                "source_generation": head.generation, "through_turn_ordinal": 1,
                "target_branch_id": fork_branch_id,
            }),
        )
        .await,
    );
    assert_eq!(
        forked.turn_count, 1,
        "a fork carries only the prefix it named"
    );

    // --- List -------------------------------------------------------------
    let listed = owner(
        &socket,
        serde_json::json!({"scope": "owner", "type": "session_list"}),
    )
    .await;
    let OwnerResponse::SessionList { page } = listed else {
        panic!("expected a Session list");
    };
    assert_eq!(page.heads.len(), 2, "the branch and its fork");

    // Half a cursor is incomplete, not shorthand for "any branch".
    let half = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_list", "after_session_id": session_id,
        }),
    )
    .await;
    assert!(
        matches!(half, OwnerResponse::Error { .. }),
        "half a paging cursor must be refused: {half:?}"
    );

    // --- History ----------------------------------------------------------
    let history = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_history",
            "session_id": session_id, "branch_id": branch_id,
            "generation": head.generation, "limit": 1,
        }),
    )
    .await;
    let OwnerResponse::SessionHistory { page } = history else {
        panic!("expected a history page");
    };
    assert_eq!(page.turns.len(), 1);
    assert_eq!(page.next_after_turn_ordinal, Some(1));

    // --- Rollback ---------------------------------------------------------
    let rolled = head_of(
        owner(
            &socket,
            serde_json::json!({
                "scope": "owner", "type": "session_rollback",
                "session_id": session_id, "branch_id": branch_id,
                "generation": head.generation, "through_turn_ordinal": 1,
            }),
        )
        .await,
    );
    assert_eq!(rolled.turn_count, 1);
    assert_eq!(
        rolled.generation,
        head.generation + 1,
        "a rollback advances the generation rather than rewriting the old one"
    );

    host.stop().await;
}

/// The lifecycle is drivable from another process.
///
/// A Controller held in-process is reachable from Tauri and from nothing that
/// runs separately, which is most of what will drive this. Without this the
/// whole lifecycle stage would be the third thing finished onto a surface the
/// desktop client cannot speak.
#[tokio::test]
async fn the_owner_socket_drives_the_lifecycle() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let host = Daemon::start(config(
        state.path().to_path_buf(),
        workspace.path().to_path_buf(),
        spawn_provider().await,
    ))
    .await;
    let socket = host.socket.clone();

    let started = owner(&socket, serde_json::json!({"scope":"owner","type":"start"})).await;
    assert!(
        matches!(started, OwnerResponse::Started),
        "expected the Runtime to open, got {started:?}"
    );
    // Asking twice is a caller that does not know it was beaten to it, not an
    // error.
    assert!(matches!(
        owner(&socket, serde_json::json!({"scope":"owner","type":"start"})).await,
        OwnerResponse::Started
    ));

    let snapshot = owner(
        &socket,
        serde_json::json!({"scope":"owner","type":"snapshot"}),
    )
    .await;
    let OwnerResponse::Snapshot {
        lifecycle,
        recovery,
        previous_shutdown,
        ..
    } = snapshot
    else {
        panic!("expected a snapshot");
    };
    assert_eq!(
        lifecycle,
        agent_runtime_host::controller::RuntimeLifecycle::Ready
    );
    assert_eq!(recovery.completed_profiles, recovery.total_profiles);
    assert!(
        previous_shutdown.is_none(),
        "a Runtime that has not shut down has nothing to hand over"
    );

    let stopped = owner(
        &socket,
        serde_json::json!({"scope":"owner","type":"shutdown"}),
    )
    .await;
    let OwnerResponse::Shutdown { report } = stopped else {
        panic!("expected a shutdown report");
    };
    assert!(!report.deadline_reached, "an idle Runtime drains at once");

    // The counts survive the call that produced them: for a desktop client the
    // caller of shutdown is a process on its way out.
    let snapshot = owner(
        &socket,
        serde_json::json!({"scope":"owner","type":"snapshot"}),
    )
    .await;
    let OwnerResponse::Snapshot {
        lifecycle,
        previous_shutdown,
        ..
    } = snapshot
    else {
        panic!("expected a snapshot");
    };
    assert_eq!(
        lifecycle,
        agent_runtime_host::controller::RuntimeLifecycle::Stopped
    );
    assert_eq!(previous_shutdown.as_ref(), Some(&*report));

    host.stop().await;
}
