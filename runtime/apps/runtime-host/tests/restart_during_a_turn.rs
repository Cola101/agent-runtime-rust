//! What the window's 重启 Runtime button does to a conversation mid-Turn.
//!
//! Two processes, because one cannot model this: a state root refuses a second
//! Runtime owner while the first still holds it, which is the property that
//! makes an in-process "restart" impossible to write honestly. A real restart
//! is `desktop/shell/electron/runtimeProcess.cjs` -- "ask it to drain, then
//! signal, then wait" -- and the second process is what recovers.
//!
//! The claim under test: a Turn in flight when the Runtime stops leaves the
//! branch owning an active Turn, and `prepare_session_continue` refuses a
//! branch that already has one. If nothing released it, the conversation would
//! be dead from then on -- and the button that did it is the one this app tells
//! people to press after changing a Provider or an MCP server.
//!
//! Codex reaches the same requirement from the other end: it writes a
//! `TurnAborted` boundary plus a model-visible marker into the durable history
//! (`codex-rs/core/src/thread_manager.rs:2145-2180`, marker text at
//! `codex-rs/core/src/context/turn_aborted.rs:9`) so that the next message
//! simply works and the model is told the previous turn may have half-executed.
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};

/// Silent for the first connection, ordinary afterwards.
///
/// The first Turn has to still be in flight when the Runtime is stopped, and
/// everything after the restart has to be able to finish -- otherwise a Run
/// that merely resumed and is working would be indistinguishable from a branch
/// that is stuck, and this test would be asserting the wrong thing.
async fn spawn_provider_silent_once() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let held = tokio::spawn(async move {
        let mut first = None;
        while let Ok((mut socket, _)) = listener.accept().await {
            if first.is_none() {
                // Held rather than dropped: closing it would let the Run fail
                // fast, which is the opposite of the state this is about.
                first = Some(socket);
                continue;
            }
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answered\"}}]}\n\n\
                            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1/chat/completions"), held)
}

fn serve(state_root: &Path, workspace: &Path, endpoint: &str) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-runtime-host"));
    command
        .arg("serve")
        .env("AGENT_RUNTIME_LOCAL_STATE_ROOT", state_root)
        .env("AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT", workspace)
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT", endpoint)
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_MODEL", "test-model")
        .env("AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY", "test-key")
        .kill_on_drop(true);
    command.spawn().expect("runtime-host serve")
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the Runtime never opened its socket at {}",
        socket.display()
    );
}

async fn owner(socket: &Path, request: serde_json::Value) -> serde_json::Value {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let line = serde_json::to_string(&request).expect("encode");
    writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("write");
    writer.flush().await.expect("flush");
    let reply = BufReader::new(reader)
        .lines()
        .next_line()
        .await
        .expect("read")
        .expect("a reply");
    serde_json::from_str(&reply).expect("decode")
}

fn socket_path(state_root: &Path) -> PathBuf {
    agent_runtime_host::ipc::default_socket_path(state_root)
}

#[tokio::test]
async fn a_runtime_restart_during_a_turn_leaves_the_conversation_continuable() {
    let state = tempfile::tempdir().expect("state root");
    let workspace = tempfile::tempdir().expect("workspace");
    let (endpoint, provider) = spawn_provider_silent_once().await;
    let socket = socket_path(state.path());
    let session_id = uuid::Uuid::now_v7();
    let branch_id = uuid::Uuid::now_v7();

    let mut host = serve(state.path(), workspace.path(), &endpoint);
    wait_for_socket(&socket).await;

    let started = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_start",
            "session_id": session_id, "branch_id": branch_id,
            "run_id": uuid::Uuid::now_v7(), "input": "the turn that gets interrupted",
        }),
    )
    .await;
    assert_eq!(
        started["type"], "session_turn",
        "the first Turn must be accepted: {started}",
    );

    // In flight: accepted, and no Turn has landed.
    let head = owner(
        &socket,
        serde_json::json!({
            "scope": "owner", "type": "session_read",
            "session_id": session_id, "branch_id": branch_id,
        }),
    )
    .await;
    assert_eq!(head["head"]["turn_count"], 0, "no Turn has completed yet");

    // The button: drain, then stop the process.
    let drained = owner(
        &socket,
        serde_json::json!({ "scope": "owner", "type": "shutdown" }),
    )
    .await;
    assert_eq!(
        drained["type"], "shutdown",
        "the owner surface must report what the drain did: {drained}",
    );
    host.kill().await.expect("stop the Runtime");
    let _ = host.wait().await;

    // The new Runtime the app starts in its place.
    let mut replacement = serve(state.path(), workspace.path(), &endpoint);
    wait_for_socket(&socket).await;

    // Bounded, and retried, because "the branch is busy for a moment while the
    // Run it owns is resumed" and "the branch is owned by a Turn nobody will
    // ever finish" look identical at one instant. Ten seconds is far longer
    // than the resumed Turn needs against a Provider that answers.
    let mut continued = serde_json::Value::Null;
    for _ in 0..100 {
        let head = owner(
            &socket,
            serde_json::json!({
                "scope": "owner", "type": "session_read",
                "session_id": session_id, "branch_id": branch_id,
            }),
        )
        .await;
        continued = owner(
            &socket,
            serde_json::json!({
                "scope": "owner", "type": "session_continue",
                "session_id": session_id, "branch_id": branch_id,
                "generation": head["head"]["generation"].clone(),
                "run_id": uuid::Uuid::now_v7(),
                "input": "the message after the restart",
            }),
        )
        .await;
        if continued["type"] != "error" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_ne!(
        continued["type"], "error",
        "a Runtime restart must not strand the conversation: {continued}",
    );

    let _ = replacement.kill().await;
    let _ = replacement.wait().await;
    provider.abort();
    let _ = provider.await;
}
