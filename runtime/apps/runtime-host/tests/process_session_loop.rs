//! Real standalone Agent loop for the durable process-session Tool protocol.
//! No Java control plane, message bus, database, container engine, or external
//! model credential participates in this acceptance path.

use agent_protocol::{RunBudget, RunStatus, RuntimeExecutionPolicySnapshot};
use agent_runtime_host::{
    LocalMcpLifecycleConfig, LocalModelRoutingConfig, LocalProcessSessionConfig,
    LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent, PROCESS_SESSION_SCOPE,
};
use agent_tool_runtime::{
    PROCESS_CLOSE_TOOL, PROCESS_POLL_TOOL, PROCESS_START_TOOL, PROCESS_WRITE_TOOL,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const PROCESS_RESIZE_TOOL: &str = "process.resize";
const PROCESS_ATTACH_TOOL: &str = "process.attach";
const PROCESS_WAIT_TOOL: &str = "process.wait";
// A bounded yield may legitimately return no output when its deadline expires.
// Keep this acceptance window well above the scripted one-second delay so a
// saturated full-workspace test run measures the Agent loop rather than macOS
// process scheduling latency.
const OUTPUT_ACCEPTANCE_YIELD_MS: u64 = 20_000;

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("line-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$$" > line.pid
trap 'rm -f line.pid' EXIT
if [ -t 0 ] && [ -t 1 ]; then printf 'terminal=yes\n'; else printf 'terminal=no\n'; fi
printf 'ready\n'
while IFS= read -r line; do
  if [ "$line" = size ]; then
    /usr/bin/python3 -c 'import fcntl,struct,termios; print(*struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))[:2])'
  else
    printf 'got:%s\n' "$line"
  fi
done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn delayed_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("delayed-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
/bin/sleep 1
printf 'delayed-ready\n'
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn delayed_interactive_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("delayed-interactive-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
/bin/sleep 1
printf 'ready\n'
while IFS= read -r line; do
  /bin/sleep 1
  printf 'got:%s\n' "$line"
done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn receipt_recovery_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("receipt-recovery-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
printf 'started\n' >> launch-count.txt
/bin/sleep 30
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn write_receipt_recovery_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("write-receipt-recovery-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
printf 'ready\n'
while IFS= read -r line; do
  printf '%s\n' "$line" >> write-count.txt
  /bin/sleep 30
  printf 'got:%s\n' "$line"
done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn close_receipt_recovery_executable_script(root: &Path) -> PathBuf {
    let executable = root.join("close-receipt-recovery-session");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$$" > close.pid
printf 'started\n' >> close-launch-count.txt
trap '' TERM INT
printf 'ready\n'
while :; do /bin/sleep 1; done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

#[cfg(unix)]
struct ProcessGroupCleanup(PathBuf);

#[cfg(unix)]
impl Drop for ProcessGroupCleanup {
    fn drop(&mut self) {
        let Ok(pid) = std::fs::read_to_string(&self.0) else {
            return;
        };
        let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
            return;
        };
        if pid > 0 {
            // SAFETY: the test script starts as its own process-group leader;
            // cleanup is scoped to that exact positive group id.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> serde_json::Value {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 8192];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "provider request ended before headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap();
            break (header_end, content_length);
        }
    };
    while request.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "provider request ended before body");
        request.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap()
}

async fn accept_nonempty_request(listener: &TcpListener) -> (TcpStream, serde_json::Value) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut first = [0_u8; 1];
        if socket.peek(&mut first).await.unwrap() == 0 {
            continue;
        }
        let request = read_request(&mut socket).await;
        return (socket, request);
    }
}

fn latest_tool_result(request: &serde_json::Value) -> serde_json::Value {
    let content = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("follow-up request has a Tool result");
    serde_json::from_str(content).unwrap()
}

fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> String {
    let delta = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

async fn respond(socket: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();
}

async fn spawn_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = Vec::new();
        let mut phase = "start";
        let mut call_sequence = 0_u32;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body = match phase {
                "start" => {
                    phase = "poll_ready";
                    calls.push(PROCESS_START_TOOL.to_owned());
                    tool_turn(
                        "process_start",
                        PROCESS_START_TOOL,
                        serde_json::json!({"tty": true, "cols": 100, "rows": 32}),
                    )
                }
                "poll_ready" => {
                    let result = latest_tool_result(&request);
                    let stdout = result["stdout"].as_str().unwrap();
                    assert!(
                        !stdout.contains("terminal=no"),
                        "the Agent Loop requested a real terminal"
                    );
                    if stdout.contains("terminal=yes") && stdout.contains("ready") {
                        phase = "write";
                    }
                    calls.push(PROCESS_POLL_TOOL.to_owned());
                    call_sequence += 1;
                    tool_turn(
                        &format!("process_poll_{call_sequence}"),
                        PROCESS_POLL_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"]
                        }),
                    )
                }
                "write" => {
                    let result = latest_tool_result(&request);
                    phase = "poll_echo";
                    calls.push(PROCESS_WRITE_TOOL.to_owned());
                    tool_turn(
                        "process_write",
                        PROCESS_WRITE_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"],
                            "stdin": "agent-loop\n"
                        }),
                    )
                }
                "poll_echo" => {
                    let result = latest_tool_result(&request);
                    if result["stdout"]
                        .as_str()
                        .unwrap()
                        .contains("got:agent-loop")
                    {
                        phase = "resize";
                    }
                    calls.push(PROCESS_POLL_TOOL.to_owned());
                    call_sequence += 1;
                    tool_turn(
                        &format!("process_poll_{call_sequence}"),
                        PROCESS_POLL_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"]
                        }),
                    )
                }
                "resize" => {
                    let result = latest_tool_result(&request);
                    phase = "write_size";
                    calls.push(PROCESS_RESIZE_TOOL.to_owned());
                    tool_turn(
                        "process_resize",
                        PROCESS_RESIZE_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"],
                            "cols": 132,
                            "rows": 43
                        }),
                    )
                }
                "write_size" => {
                    let result = latest_tool_result(&request);
                    phase = "poll_size";
                    calls.push(PROCESS_WRITE_TOOL.to_owned());
                    tool_turn(
                        "process_write_size",
                        PROCESS_WRITE_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"],
                            "stdin": "size\n"
                        }),
                    )
                }
                "poll_size" => {
                    let result = latest_tool_result(&request);
                    if result["stdout"].as_str().unwrap().contains("43 132") {
                        phase = "attach";
                    }
                    calls.push(PROCESS_POLL_TOOL.to_owned());
                    call_sequence += 1;
                    tool_turn(
                        &format!("process_poll_{call_sequence}"),
                        PROCESS_POLL_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"]
                        }),
                    )
                }
                "attach" => {
                    let result = latest_tool_result(&request);
                    phase = "verify_attach";
                    calls.push(PROCESS_ATTACH_TOOL.to_owned());
                    tool_turn(
                        "process_attach",
                        PROCESS_ATTACH_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "max_bytes": 24
                        }),
                    )
                }
                "verify_attach" => {
                    let result = latest_tool_result(&request);
                    assert!(result["stdout"].as_str().unwrap().len() <= 24);
                    assert!(result["stdout"].as_str().unwrap().contains("43 132"));
                    assert_eq!(result["stdout_truncated"], true);
                    assert!(result["stdout_start_cursor"].as_u64().unwrap() > 0);
                    phase = "finish";
                    calls.push(PROCESS_CLOSE_TOOL.to_owned());
                    tool_turn(
                        "process_close",
                        PROCESS_CLOSE_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"]
                        }),
                    )
                }
                "close" => {
                    let result = latest_tool_result(&request);
                    phase = "finish";
                    calls.push(PROCESS_CLOSE_TOOL.to_owned());
                    tool_turn(
                        "process_close",
                        PROCESS_CLOSE_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"]
                        }),
                    )
                }
                "finish" => {
                    let result = latest_tool_result(&request);
                    assert!(matches!(
                        result["state"].as_str(),
                        Some("exited" | "terminated")
                    ));
                    respond(&mut socket, &text_turn("persistent process complete")).await;
                    break;
                }
                _ => unreachable!(),
            };
            respond(&mut socket, &body).await;
        }
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn process_tool_schema<'a>(request: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    request["tools"]
        .as_array()
        .expect("model request exposes Tool definitions")
        .iter()
        .find(|tool| tool["function"]["name"] == name)
        .map(|tool| &tool["function"]["parameters"])
        .unwrap_or_else(|| panic!("model request is missing {name}"))
}

async fn spawn_unified_yield_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = Vec::new();

        let (mut first, _) = listener.accept().await.unwrap();
        let first_request = read_request(&mut first).await;
        for name in [PROCESS_START_TOOL, PROCESS_WRITE_TOOL] {
            assert!(
                process_tool_schema(&first_request, name)["properties"]["yield_time_ms"]
                    .is_object(),
                "{name} must expose bounded yield to the model"
            );
        }
        calls.push(PROCESS_START_TOOL.to_owned());
        respond(
            &mut first,
            &tool_turn(
                "unified_start",
                PROCESS_START_TOOL,
                serde_json::json!({"yield_time_ms": OUTPUT_ACCEPTANCE_YIELD_MS}),
            ),
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        let second_request = read_request(&mut second).await;
        let started = latest_tool_result(&second_request);
        assert_eq!(started["stdout"], "ready\n");
        calls.push(PROCESS_WRITE_TOOL.to_owned());
        respond(
            &mut second,
            &tool_turn(
                "unified_write",
                PROCESS_WRITE_TOOL,
                serde_json::json!({
                    "session_id": started["session_id"],
                    "stdout_cursor": started["stdout_cursor"],
                    "stderr_cursor": started["stderr_cursor"],
                    "stdin": "agent-loop\n",
                    "yield_time_ms": OUTPUT_ACCEPTANCE_YIELD_MS
                }),
            ),
        )
        .await;

        let (mut third, _) = listener.accept().await.unwrap();
        let third_request = read_request(&mut third).await;
        let written = latest_tool_result(&third_request);
        assert_eq!(written["stdout"], "got:agent-loop\n");
        calls.push(PROCESS_CLOSE_TOOL.to_owned());
        respond(
            &mut third,
            &tool_turn(
                "unified_close",
                PROCESS_CLOSE_TOOL,
                serde_json::json!({
                    "session_id": written["session_id"],
                    "stdout_cursor": written["stdout_cursor"],
                    "stderr_cursor": written["stderr_cursor"]
                }),
            ),
        )
        .await;

        let (mut fourth, _) = listener.accept().await.unwrap();
        let fourth_request = read_request(&mut fourth).await;
        let closed = latest_tool_result(&fourth_request);
        assert!(matches!(
            closed["state"].as_str(),
            Some("exited" | "terminated")
        ));
        respond(&mut fourth, &text_turn("unified yield complete")).await;
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_start_receipt_recovery_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = vec![PROCESS_START_TOOL.to_owned()];
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut first).await;
        respond(
            &mut first,
            &tool_turn(
                "receipt_start",
                PROCESS_START_TOOL,
                serde_json::json!({"yield_time_ms": 30_000}),
            ),
        )
        .await;

        let (mut recovered, recovered_request) = accept_nonempty_request(&listener).await;
        let started = latest_tool_result(&recovered_request);
        assert!(started["session_id"].as_str().is_some());
        calls.push(PROCESS_CLOSE_TOOL.to_owned());
        respond(
            &mut recovered,
            &tool_turn(
                "receipt_close",
                PROCESS_CLOSE_TOOL,
                serde_json::json!({
                    "session_id": started["session_id"],
                    "stdout_cursor": started["stdout_cursor"],
                    "stderr_cursor": started["stderr_cursor"]
                }),
            ),
        )
        .await;

        let (mut finished, finished_request) = accept_nonempty_request(&listener).await;
        let closed = latest_tool_result(&finished_request);
        assert!(matches!(
            closed["state"].as_str(),
            Some("exited" | "terminated")
        ));
        respond(&mut finished, &text_turn("start receipt recovered")).await;
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_write_receipt_recovery_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = vec![PROCESS_START_TOOL.to_owned()];
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut first).await;
        respond(
            &mut first,
            &tool_turn(
                "write_receipt_start",
                PROCESS_START_TOOL,
                serde_json::json!({"yield_time_ms": 5_000}),
            ),
        )
        .await;

        let (mut writing, _) = listener.accept().await.unwrap();
        let writing_request = read_request(&mut writing).await;
        let started = latest_tool_result(&writing_request);
        calls.push(PROCESS_WRITE_TOOL.to_owned());
        respond(
            &mut writing,
            &tool_turn(
                "write_receipt_write",
                PROCESS_WRITE_TOOL,
                serde_json::json!({
                    "session_id": started["session_id"],
                    "stdout_cursor": started["stdout_cursor"],
                    "stderr_cursor": started["stderr_cursor"],
                    "stdin": "write-once\n",
                    "yield_time_ms": 30_000
                }),
            ),
        )
        .await;

        let (mut recovered, recovered_request) = accept_nonempty_request(&listener).await;
        let written = latest_tool_result(&recovered_request);
        assert!(written["session_id"].as_str().is_some());
        calls.push(PROCESS_CLOSE_TOOL.to_owned());
        respond(
            &mut recovered,
            &tool_turn(
                "write_receipt_close",
                PROCESS_CLOSE_TOOL,
                serde_json::json!({
                    "session_id": written["session_id"],
                    "stdout_cursor": written["stdout_cursor"],
                    "stderr_cursor": written["stderr_cursor"]
                }),
            ),
        )
        .await;

        let (mut finished, finished_request) = accept_nonempty_request(&listener).await;
        let closed = latest_tool_result(&finished_request);
        assert!(matches!(
            closed["state"].as_str(),
            Some("exited" | "terminated")
        ));
        respond(&mut finished, &text_turn("write receipt recovered")).await;
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_close_receipt_recovery_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = vec![PROCESS_START_TOOL.to_owned()];
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut first).await;
        respond(
            &mut first,
            &tool_turn(
                "close_receipt_start",
                PROCESS_START_TOOL,
                serde_json::json!({"yield_time_ms": OUTPUT_ACCEPTANCE_YIELD_MS}),
            ),
        )
        .await;

        let (mut closing, _) = listener.accept().await.unwrap();
        let closing_request = read_request(&mut closing).await;
        let started = latest_tool_result(&closing_request);
        assert_eq!(started["stdout"], "ready\n");
        calls.push(PROCESS_CLOSE_TOOL.to_owned());
        respond(
            &mut closing,
            &tool_turn(
                "close_receipt_close",
                PROCESS_CLOSE_TOOL,
                serde_json::json!({
                    "session_id": started["session_id"],
                    "stdout_cursor": started["stdout_cursor"],
                    "stderr_cursor": started["stderr_cursor"]
                }),
            ),
        )
        .await;

        let (mut recovered, recovered_request) = accept_nonempty_request(&listener).await;
        let closed = latest_tool_result(&recovered_request);
        assert_eq!(closed["state"], "terminated");
        assert_eq!(closed["termination_reason"], "closed");
        respond(&mut recovered, &text_turn("close receipt recovered")).await;
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_wait_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = Vec::new();
        let mut phase = "start";
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let body = match phase {
                "start" => {
                    phase = "wait";
                    calls.push(PROCESS_START_TOOL.to_owned());
                    tool_turn(
                        "process_start",
                        PROCESS_START_TOOL,
                        serde_json::json!({"tty": false}),
                    )
                }
                "wait" => {
                    let result = latest_tool_result(&request);
                    assert_eq!(result["state"], "running");
                    assert!(result["stdout"].as_str().unwrap().is_empty());
                    phase = "finish";
                    calls.push(PROCESS_WAIT_TOOL.to_owned());
                    tool_turn(
                        "process_wait",
                        PROCESS_WAIT_TOOL,
                        serde_json::json!({
                            "session_id": result["session_id"],
                            "stdout_cursor": result["stdout_cursor"],
                            "stderr_cursor": result["stderr_cursor"],
                            "yield_time_ms": OUTPUT_ACCEPTANCE_YIELD_MS
                        }),
                    )
                }
                "finish" => {
                    let result = latest_tool_result(&request);
                    assert!(
                        matches!(result["state"].as_str(), Some("running" | "exited")),
                        "{result:#}"
                    );
                    assert_eq!(result["stdout"], "delayed-ready\n");
                    respond(&mut socket, &text_turn("yielded process complete")).await;
                    break;
                }
                _ => unreachable!(),
            };
            respond(&mut socket, &body).await;
        }
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

async fn spawn_recovery_provider() -> (
    String,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (checkpoint_reached_tx, checkpoint_reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut calls = vec![PROCESS_START_TOOL.to_owned()];
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut first).await;
        respond(
            &mut first,
            &tool_turn(
                "recovery_start",
                PROCESS_START_TOOL,
                serde_json::json!({"tty": true, "cols": 100, "rows": 32}),
            ),
        )
        .await;

        // Receipt proves the start result crossed the Tool ambiguity boundary
        // and was fed back into the next model invocation.
        let (mut interrupted, _) = listener.accept().await.unwrap();
        let interrupted_request = read_request(&mut interrupted).await;
        let mut last_result = latest_tool_result(&interrupted_request);
        checkpoint_reached_tx.send(()).unwrap();
        let _ = release_rx.await;
        drop(interrupted);

        let mut phase = "write";
        let mut sequence = 0_u32;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            if request["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"))
            {
                last_result = latest_tool_result(&request);
            }
            let body = match phase {
                "write" => {
                    phase = "observe_echo";
                    calls.push(PROCESS_WRITE_TOOL.to_owned());
                    tool_turn(
                        "recovery_write",
                        PROCESS_WRITE_TOOL,
                        serde_json::json!({
                            "session_id": last_result["session_id"],
                            "stdout_cursor": last_result["stdout_cursor"],
                            "stderr_cursor": last_result["stderr_cursor"],
                            "stdin": "after-host-restart\n"
                        }),
                    )
                }
                "observe_echo" => {
                    if last_result["stdout"]
                        .as_str()
                        .unwrap()
                        .contains("got:after-host-restart")
                    {
                        phase = "verify_attach";
                        calls.push(PROCESS_ATTACH_TOOL.to_owned());
                        tool_turn(
                            "recovery_attach",
                            PROCESS_ATTACH_TOOL,
                            serde_json::json!({
                                "session_id": last_result["session_id"],
                                "max_bytes": 32
                            }),
                        )
                    } else {
                        calls.push(PROCESS_POLL_TOOL.to_owned());
                        sequence += 1;
                        tool_turn(
                            &format!("recovery_poll_{sequence}"),
                            PROCESS_POLL_TOOL,
                            serde_json::json!({
                                "session_id": last_result["session_id"],
                                "stdout_cursor": last_result["stdout_cursor"],
                                "stderr_cursor": last_result["stderr_cursor"]
                            }),
                        )
                    }
                }
                "verify_attach" => {
                    assert!(
                        last_result["stdout"]
                            .as_str()
                            .unwrap()
                            .contains("got:after-host-restart")
                    );
                    assert_eq!(last_result["stdout_truncated"], true);
                    phase = "finish_close";
                    calls.push(PROCESS_CLOSE_TOOL.to_owned());
                    tool_turn(
                        "recovery_close",
                        PROCESS_CLOSE_TOOL,
                        serde_json::json!({
                            "session_id": last_result["session_id"],
                            "stdout_cursor": last_result["stdout_cursor"],
                            "stderr_cursor": last_result["stderr_cursor"]
                        }),
                    )
                }
                "finish_close" => {
                    assert!(matches!(
                        last_result["state"].as_str(),
                        Some("exited" | "terminated")
                    ));
                    respond(&mut socket, &text_turn("recovered process complete")).await;
                    break;
                }
                _ => unreachable!(),
            };
            respond(&mut socket, &body).await;
        }
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        checkpoint_reached_rx,
        release_tx,
        handle,
    )
}

async fn spawn_governance_provider() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let mut calls = Vec::new();

        let (mut start_socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut start_socket).await;
        calls.push(PROCESS_START_TOOL.to_owned());
        respond(
            &mut start_socket,
            &tool_turn("governed_start", PROCESS_START_TOOL, serde_json::json!({})),
        )
        .await;

        let (mut delayed_socket, _) = listener.accept().await.unwrap();
        let delayed_request = read_request(&mut delayed_socket).await;
        let started = latest_tool_result(&delayed_request);
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        calls.push(PROCESS_POLL_TOOL.to_owned());
        respond(
            &mut delayed_socket,
            &tool_turn(
                "governed_poll",
                PROCESS_POLL_TOOL,
                serde_json::json!({
                    "session_id": started["session_id"],
                    "stdout_cursor": started["stdout_cursor"],
                    "stderr_cursor": started["stderr_cursor"]
                }),
            ),
        )
        .await;

        let (mut finish_socket, _) = listener.accept().await.unwrap();
        let finish_request = read_request(&mut finish_socket).await;
        let terminated = latest_tool_result(&finish_request);
        assert_eq!(terminated["state"], "terminated");
        assert_eq!(terminated["termination_reason"], "execution_deadline");
        respond(&mut finish_socket, &text_turn("governed process complete")).await;
        calls
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn runtime_config(
    state_root: PathBuf,
    workspace_root: PathBuf,
    endpoint: String,
    executable: PathBuf,
) -> LocalRuntimeConfig {
    let mut model_routing = LocalModelRoutingConfig::single_openai_compatible(
        endpoint,
        "loopback-model",
        "loopback-key",
    );
    // One initial invocation plus one replacement-Host retry. The default of
    // one attempt intentionally refuses an interrupted Provider call.
    model_routing.health_policy.max_same_provider_attempts = 2;
    LocalRuntimeConfig {
        state_root,
        workspace_root,
        agent_instructions: "Use the process Tools exactly as requested.".into(),
        delegated_scopes: BTreeSet::from([PROCESS_SESSION_SCOPE.to_owned()]),
        subagent_roles: Vec::new(),
        model_routing,
        mcp_servers: Vec::new(),
        mcp_lifecycle: LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: None,
        process_session: Some(LocalProcessSessionConfig {
            executable,
            fixed_args: Vec::new(),
            max_output_chunk_bytes: 16 * 1024,
            governance: agent_tool_runtime::ProcessSessionGovernance::default(),
            pty_supervisor: Some(agent_tool_runtime::ProcessSessionPtySupervisorConfig {
                executable: PathBuf::from(env!("CARGO_BIN_EXE_agent-runtime-host")),
                fixed_args: vec!["__pty-session-supervisor".into()],
                startup_timeout: std::time::Duration::from_secs(5),
            }),
        }),
        consent: LocalToolConsent::AllowOnce,
        budget: RunBudget {
            max_tokens: 8_192,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: RuntimeExecutionPolicySnapshot::default(),
    }
}

#[tokio::test]
async fn standalone_agent_loop_starts_writes_polls_and_closes_a_real_process() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    #[cfg(unix)]
    let _cleanup = ProcessGroupCleanup(workspace.path().join("line.pid"));
    let (endpoint, provider) = spawn_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    ))
    .unwrap();

    let outcome = host
        .execute("Run the persistent process session.")
        .await
        .unwrap();
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "{outcome:?}; events={events:#?}"
    );
    assert_eq!(outcome.output, "persistent process complete");
    assert!(outcome.checkpoint_path.is_file());
    let calls = provider.await.unwrap();
    assert_eq!(calls.first().map(String::as_str), Some(PROCESS_START_TOOL));
    assert!(calls.iter().any(|name| name == PROCESS_WRITE_TOOL));
    assert!(calls.iter().any(|name| name == PROCESS_POLL_TOOL));
    assert!(calls.iter().any(|name| name == PROCESS_RESIZE_TOOL));
    assert!(calls.iter().any(|name| name == PROCESS_ATTACH_TOOL));
    assert_eq!(calls.last().map(String::as_str), Some(PROCESS_CLOSE_TOOL));
}

#[tokio::test]
async fn standalone_agent_loop_starts_and_writes_with_bounded_yield_without_model_polling() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = delayed_interactive_executable_script(trusted.path());
    let (endpoint, provider) = spawn_unified_yield_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    ))
    .unwrap();

    let outcome = host
        .execute("Start the delayed process, send input, and use bounded yield.")
        .await
        .unwrap();
    let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "{outcome:?}; events={events:#?}"
    );
    assert_eq!(outcome.output, "unified yield complete");
    assert_eq!(
        provider.await.unwrap(),
        vec![
            PROCESS_START_TOOL.to_owned(),
            PROCESS_WRITE_TOOL.to_owned(),
            PROCESS_CLOSE_TOOL.to_owned()
        ]
    );
}

/// The production break this catches is a replacement Host turning a proven
/// durable `process.start` session into indeterminate merely because the first
/// Host died while waiting for yielded output.
#[tokio::test]
async fn replacement_host_recovers_the_started_session_from_a_lost_start_yield_result() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = receipt_recovery_executable_script(trusted.path());
    let (endpoint, provider) = spawn_start_receipt_recovery_provider().await;
    let config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let first_config = config.clone();
    let run_id = uuid::Uuid::now_v7();
    let input = "Start the process and recover its accepted result.";
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(run_id, input).await
    });

    let launch_count = workspace.path().join("launch-count.txt");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !launch_count.is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process start never crossed its external effect boundary");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running.abort();
    let _ = running.await;

    let mut replacement = LocalRuntimeHost::start(config).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        replacement.resume(run_id, input, 2),
    )
    .await
    .expect("replacement Host did not settle the accepted start")
    .unwrap();
    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.output, "start receipt recovered");
    assert_eq!(
        std::fs::read_to_string(&launch_count).unwrap(),
        "started\n",
        "recovery must never launch a duplicate process"
    );
    assert_eq!(
        provider.await.unwrap(),
        vec![PROCESS_START_TOOL.to_owned(), PROCESS_CLOSE_TOOL.to_owned()]
    );
}

/// The production break this catches is a replacement Host either losing an
/// acknowledged process write or replaying its stdin after the original Host
/// died while waiting for yielded output.
#[tokio::test]
async fn replacement_host_recovers_one_committed_write_without_sending_stdin_twice() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = write_receipt_recovery_executable_script(trusted.path());
    let (endpoint, provider) = spawn_write_receipt_recovery_provider().await;
    let config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let first_config = config.clone();
    let run_id = uuid::Uuid::now_v7();
    let input = "Write to the process exactly once and recover its accepted result.";
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(run_id, input).await
    });

    let write_count = workspace.path().join("write-count.txt");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !write_count.is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process write never crossed its external effect boundary");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let receipts = std::fs::read_dir(
        state
            .path()
            .join("tool-process-session-state/process-sessions")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("interaction-receipts"),
    )
    .expect("committed process write has no durable receipt")
    .filter_map(Result::ok)
    .count();
    assert_eq!(receipts, 1, "one write must create exactly one receipt");
    running.abort();
    let _ = running.await;

    let mut replacement = LocalRuntimeHost::start(config).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        replacement.resume(run_id, input, 2),
    )
    .await
    .expect("replacement Host did not settle the accepted write")
    .unwrap();
    let events = LocalRuntimeHost::replay_events(state.path(), run_id, 0).unwrap();
    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "{outcome:?}; events={events:#?}"
    );
    assert_eq!(outcome.output, "write receipt recovered");
    assert_eq!(
        std::fs::read_to_string(&write_count).unwrap(),
        "write-once\n",
        "recovery must never send the same stdin twice"
    );
    assert_eq!(
        provider.await.unwrap(),
        vec![
            PROCESS_START_TOOL.to_owned(),
            PROCESS_WRITE_TOOL.to_owned(),
            PROCESS_CLOSE_TOOL.to_owned()
        ]
    );
}

/// The production break this catches is a replacement Host abandoning a
/// durable close intent, or asking the model to issue a second close, after
/// the first Host dies while the identity-fenced termination is in progress.
#[cfg(unix)]
#[tokio::test]
async fn replacement_host_finishes_one_durable_close_intent_and_returns_its_terminal_result() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = close_receipt_recovery_executable_script(trusted.path());
    let (endpoint, provider) = spawn_close_receipt_recovery_provider().await;
    let config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let first_config = config.clone();
    let run_id = uuid::Uuid::now_v7();
    let input = "Close the process once and recover the terminal result.";
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(run_id, input).await
    });
    let _cleanup = ProcessGroupCleanup(workspace.path().join("close.pid"));

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let sessions = state
                .path()
                .join("tool-process-session-state/process-sessions");
            let terminating = std::fs::read_dir(sessions).ok().is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    std::fs::read(entry.path().join("manifest.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .is_some_and(|value| value["manifest"]["state"] == "terminating")
                })
            });
            if terminating {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("close never reached its durable terminating intent");
    running.abort();
    let _ = running.await;

    let mut replacement = LocalRuntimeHost::start(config).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        replacement.resume(run_id, input, 2),
    )
    .await
    .expect("replacement Host did not settle the durable close intent")
    .unwrap();
    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.output, "close receipt recovered");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("close-launch-count.txt")).unwrap(),
        "started\n",
        "close recovery must not restart the process"
    );
    assert_eq!(
        provider.await.unwrap(),
        vec![PROCESS_START_TOOL.to_owned(), PROCESS_CLOSE_TOOL.to_owned()]
    );
}

#[tokio::test]
async fn standalone_agent_loop_yields_until_delayed_process_output_without_model_polling() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = delayed_executable_script(trusted.path());
    let (endpoint, provider) = spawn_wait_provider().await;
    let mut host = LocalRuntimeHost::start(runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    ))
    .unwrap();

    let outcome = host
        .execute("Run the delayed process and wait for its output.")
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.output, "yielded process complete");
    assert_eq!(
        provider.await.unwrap(),
        vec![PROCESS_START_TOOL.to_owned(), PROCESS_WAIT_TOOL.to_owned()]
    );
}

#[tokio::test]
async fn replacement_host_resumes_a_started_pure_wait_without_restarting_the_process() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = delayed_executable_script(trusted.path());
    let (endpoint, provider) = spawn_wait_provider().await;
    let config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let first_config = config.clone();
    let run_id = uuid::Uuid::now_v7();
    let input = "Run the delayed process and survive replacement while waiting.";
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(run_id, input).await
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let started_count = LocalRuntimeHost::replay_events(state.path(), run_id, 0)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == "tool.execution.started")
            .count();
        if started_count >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "process.wait never reached its durable started boundary"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(LocalRuntimeHost::checkpoint_path(state.path(), run_id).is_file());
    running.abort();
    let _ = running.await;

    let mut replacement = LocalRuntimeHost::start(config).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        replacement.resume(run_id, input, 2),
    )
    .await
    .expect("replacement Host did not finish the durable wait")
    .unwrap();
    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.output, "yielded process complete");
    let calls = provider.await.unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|name| name.as_str() == PROCESS_START_TOOL)
            .count(),
        1,
        "replacement must not start a second child"
    );
    assert_eq!(calls, vec![PROCESS_START_TOOL, PROCESS_WAIT_TOOL]);
}

#[tokio::test]
async fn replacement_host_resumes_after_the_start_result_without_restarting_the_process() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    #[cfg(unix)]
    let _cleanup = ProcessGroupCleanup(workspace.path().join("line.pid"));
    let (endpoint, checkpoint_reached, release, provider) = spawn_recovery_provider().await;
    let config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let first_config = config.clone();
    let run_id = uuid::Uuid::now_v7();
    let input = "Recover the persistent process session.";
    let running = tokio::spawn(async move {
        let mut host = LocalRuntimeHost::start(first_config).unwrap();
        host.execute_as(run_id, input).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), checkpoint_reached)
        .await
        .expect("start result never reached the next model request")
        .unwrap();
    assert!(LocalRuntimeHost::checkpoint_path(state.path(), run_id).is_file());
    running.abort();
    let _ = running.await;
    release.send(()).unwrap();

    let mut replacement = LocalRuntimeHost::start(config).unwrap();
    let outcome = replacement.resume(run_id, input, 2).await.unwrap();
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "recovered process complete");
    let calls = provider.await.unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|name| name.as_str() == PROCESS_START_TOOL)
            .count(),
        1,
        "a replacement Host must not start a second process"
    );
    assert!(calls.iter().any(|name| name == PROCESS_WRITE_TOOL));
    assert!(calls.iter().any(|name| name == PROCESS_ATTACH_TOOL));
    assert_eq!(calls.last().map(String::as_str), Some(PROCESS_CLOSE_TOOL));
}

#[tokio::test]
async fn standalone_agent_loop_observes_a_persisted_execution_deadline() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    #[cfg(unix)]
    let _cleanup = ProcessGroupCleanup(workspace.path().join("line.pid"));
    let (endpoint, provider) = spawn_governance_provider().await;
    let mut config = runtime_config(
        state.path().to_path_buf(),
        workspace.path().canonicalize().unwrap(),
        endpoint,
        executable,
    );
    let governance = &mut config.process_session.as_mut().unwrap().governance;
    governance.max_runtime = std::time::Duration::from_millis(100);
    governance.idle_timeout = std::time::Duration::from_secs(5);
    let mut host = LocalRuntimeHost::start(config).unwrap();

    let outcome = host
        .execute("Start the process and report its governed terminal state.")
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.output, "governed process complete");
    assert!(outcome.checkpoint_path.is_file());
    assert_eq!(
        provider.await.unwrap(),
        vec![PROCESS_START_TOOL.to_owned(), PROCESS_POLL_TOOL.to_owned()]
    );
}
