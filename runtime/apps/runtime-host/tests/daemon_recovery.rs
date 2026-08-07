//! Daemon-restart recovery (ADR-0035 decision 7, durability half).
//!
//! A client dying must not kill a Run — that is already covered. This file
//! covers the harder half: the daemon itself dying. The Checkpoint is on disk,
//! so a restarted daemon must pick the Run back up instead of leaving it
//! stranded, and must never re-execute a Run that already finished.

use agent_protocol::RunBudget;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalProviderConfig, LocalRunState, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use uuid::Uuid;

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Provider that strands its first caller and answers every later one. The
/// stranded connection stands in for a daemon that died mid-turn.
async fn spawn_stranding_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let mut served = 0u32;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut buffer).await;
            served += 1;
            if served == 1 {
                // Hold the connection open forever without answering.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    drop(socket);
                });
                continue;
            }
            let body = text_turn("recovered answer");
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
        agent_instructions: "Explain evidence before conclusions.".into(),
        delegated_scopes: BTreeSet::from([WORKSPACE_READ_SCOPE.to_owned()]),
        provider: LocalProviderConfig {
            endpoint,
            model: "local-test-model".into(),
            api_key: "local-test-key".into(),
        },
        trusted_workspace_tool: None,
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 4_096,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
    }
}

async fn submit(socket: &Path, input: &str) -> Uuid {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(&LocalRequest::Submit {
        input: input.into(),
    })
    .expect("encode");
    line.push(b'\n');
    writer.write_all(&line).await.expect("write");
    writer.flush().await.expect("flush");
    let mut lines = BufReader::new(reader).lines();
    let response: LocalResponse =
        serde_json::from_str(&lines.next_line().await.expect("read").expect("line"))
            .expect("decode");
    match response {
        LocalResponse::Accepted { run_id } => run_id,
        other => panic!("expected acceptance, got {other:?}"),
    }
}

async fn wait_for<F: Fn() -> bool>(label: &str, predicate: F) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {label}");
}

/// Runs a daemon on its own runtime, submits one Run, waits for it to become
/// durable, then drops the runtime. Dropping aborts every task the daemon
/// spawned, which is as close to a crash as a test can get in-process.
fn crash_after_first_checkpoint(config: LocalRuntimeConfig) -> Uuid {
    let state_root = config.state_root.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let socket = default_socket_path(&config.state_root);
            let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
            let daemon = LocalRuntimeDaemon::new(config);
            tokio::spawn(daemon.serve(listener));
            let run_id = submit(&socket, "Summarize the workspace.").await;
            wait_for("the run to become durable", || {
                LocalRuntimeHost::checkpoint_path(&state_root, run_id).is_file()
            })
            .await;
            run_id
        })
        // The runtime is dropped here: the daemon and its in-flight Run die.
    })
    .join()
    .expect("daemon thread")
}

#[tokio::test]
async fn a_restarted_daemon_resumes_a_run_its_predecessor_left_unfinished() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    let endpoint = spawn_stranding_provider().await;

    let run_id = crash_after_first_checkpoint(config(
        state_root.clone(),
        workspace_root.clone(),
        endpoint.clone(),
    ));

    // The predecessor left the Run marked running with a Checkpoint on disk.
    let stranded = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("the crashed daemon recorded the run");
    assert_eq!(stranded.state, LocalRunState::Running);

    // A replacement daemon on the same state root must pick it back up.
    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    daemon.recover_unfinished().await.expect("recovery runs");
    tokio::spawn(daemon.serve(listener));

    let probe = state_root.clone();
    wait_for("the recovered run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if !matches!(record.state, LocalRunState::Running)
        )
    })
    .await;

    let recovered = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("record readable")
        .expect("record present");
    assert_eq!(
        recovered.state,
        LocalRunState::Finished {
            status: "succeeded".into()
        },
        "the replacement daemon must finish the stranded Run"
    );
    assert!(
        recovered.owner_epoch > stranded.owner_epoch,
        "recovery must take a strictly newer owner epoch: {} -> {}",
        stranded.owner_epoch,
        recovered.owner_epoch
    );
}

#[tokio::test]
async fn a_restarted_daemon_never_re_executes_a_run_that_already_finished() {
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let state_root = state.path().to_path_buf();
    let workspace_root = workspace.path().canonicalize().expect("canonical");
    // Exactly one turn is available: a second execution would strand forever.
    let endpoint = spawn_stranding_provider().await;

    // Burn the stranding first connection so the next call succeeds.
    let _ = tokio::net::TcpStream::connect(
        endpoint
            .trim_start_matches("http://")
            .trim_end_matches("/v1/chat/completions"),
    )
    .await;

    let socket = default_socket_path(&state_root);
    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    let daemon = LocalRuntimeDaemon::new(config(
        state_root.clone(),
        workspace_root.clone(),
        endpoint.clone(),
    ));
    tokio::spawn(daemon.serve(listener));
    let run_id = submit(&socket, "Summarize the workspace.").await;

    let probe = state_root.clone();
    wait_for("the run to finish", move || {
        matches!(
            LocalRuntimeHost::read_run_record(&probe, run_id),
            Ok(Some(record)) if matches!(record.state, LocalRunState::Finished { .. })
        )
    })
    .await;
    let finished = LocalRuntimeHost::read_run_record(&state_root, run_id)
        .expect("readable")
        .expect("present");
    let events_before =
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable");

    // A replacement daemon must leave the finished Run alone.
    let replacement = LocalRuntimeDaemon::new(config(state_root.clone(), workspace_root, endpoint));
    replacement
        .recover_unfinished()
        .await
        .expect("recovery runs");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        LocalRuntimeHost::read_run_record(&state_root, run_id)
            .expect("readable")
            .expect("present"),
        finished,
        "a finished Run must not be touched by recovery"
    );
    assert_eq!(
        LocalRuntimeHost::replay_events(&state_root, run_id, 0).expect("events readable"),
        events_before,
        "recovery must not append events to a finished Run"
    );
}
