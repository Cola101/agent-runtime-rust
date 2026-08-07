//! `shell.exec` behaviour, driven through the tool's real stdin/stdout contract.
//!
//! A Shell Tool hands arbitrary command execution to the model, so the inner
//! boundary carries more weight here than it does for read/write: Seatbelt
//! contains what the command can reach, and this tool decides what the command
//! starts with -- which directory, which environment, which limits.
//!
//! These tests run the tool *without* Seatbelt on purpose. Containment has its
//! own tests; what is under test here is the inner boundary alone, so a gap in
//! either one does not depend on the other to stay closed.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tool_binary() -> PathBuf {
    let mut current = std::env::current_exe().expect("test binary path");
    while current.pop() {
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("agent-trusted-workspace-tool must be built");
}

/// Returns a guard, not a path: dropping it removes the directory, so a run
/// of this suite leaves nothing behind.
fn workspace(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("agent-shell-tool-{label}-"))
        .tempdir()
        .expect("workspace")
}

/// Runs the tool as the executor does, and with a secret in the parent
/// environment so the tests can prove it is not passed through.
fn invoke(workspace: &Path, arguments: Value) -> Value {
    let mut child = Command::new(tool_binary())
        .arg("--stdio")
        .current_dir(workspace)
        .env(
            "AGENT_RUNTIME_PROVIDER_API_KEY",
            "parent-secret-must-not-leak",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tool");
    let request = json!({
        "schema_version": 1,
        "tool_call": {"id": "call_shell_1", "name": "shell.exec", "arguments": arguments},
        "binding_digest": "c".repeat(64),
    });
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(serde_json::to_vec(&request).unwrap().as_slice())
        .expect("write request");
    let output = child.wait_with_output().expect("tool output");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "tool did not emit a JSON result; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn a_command_runs_and_reports_its_output_and_exit_status() {
    let workspace = workspace("basic");
    let result = invoke(
        workspace.path(),
        json!({"command": "echo hello-from-shell"}),
    );

    assert_eq!(result["is_error"], json!(false), "{result}");
    assert_eq!(result["content"]["exit_code"], json!(0), "{result}");
    assert!(
        result["content"]["stdout"]
            .as_str()
            .unwrap()
            .contains("hello-from-shell"),
        "{result}"
    );
}

/// A non-zero exit is a real result the model must see, not a tool failure.
/// Collapsing the two would make "the command failed" indistinguishable from
/// "the Tool could not run it", and the second is what fail-closed applies to.
#[test]
fn a_failing_command_reports_its_status_without_becoming_a_tool_error() {
    let workspace = workspace("exit-status");
    let result = invoke(
        workspace.path(),
        json!({"command": "echo to-stderr >&2; exit 3"}),
    );

    assert_eq!(result["is_error"], json!(false), "{result}");
    assert_eq!(result["content"]["exit_code"], json!(3), "{result}");
    assert!(
        result["content"]["stderr"]
            .as_str()
            .unwrap()
            .contains("to-stderr"),
        "{result}"
    );
}

#[test]
fn the_command_starts_in_the_workspace() {
    let workspace = workspace("cwd");
    std::fs::write(workspace.path().join("marker.txt"), "found\n").unwrap();
    let result = invoke(workspace.path(), json!({"command": "cat marker.txt"}));

    assert!(
        result["content"]["stdout"]
            .as_str()
            .unwrap()
            .contains("found"),
        "{result}"
    );
}

/// The Worker process holds provider credentials, database passwords and NATS
/// credentials. None of it may reach a model-authored command.
#[test]
fn the_parent_environment_never_reaches_the_command() {
    let workspace = workspace("env");
    let result = invoke(
        workspace.path(),
        json!({"command": "echo \"key=[${AGENT_RUNTIME_PROVIDER_API_KEY:-unset}]\"; env | wc -l"}),
    );

    let stdout = result["content"]["stdout"].as_str().unwrap();
    assert!(
        stdout.contains("key=[unset]"),
        "a parent environment variable reached the command: {result}"
    );
    assert!(
        !stdout.contains("parent-secret-must-not-leak"),
        "the parent secret leaked into the command environment: {result}"
    );
}

/// Measured: pointing HOME at the Workspace root makes macOS frameworks create
/// `Library/Caches/...` there, so the model's own directory fills with junk.
/// HOME therefore points at a dot-directory inside the Workspace -- still
/// contained, and `~` never resolves to the real home.
#[test]
fn home_points_inside_the_workspace_and_not_at_the_real_home() {
    let workspace = workspace("home");
    let result = invoke(workspace.path(), json!({"command": "echo \"$HOME\""}));

    let home = result["content"]["stdout"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    let real_home = std::env::var("HOME").unwrap_or_default();
    assert!(
        !home.is_empty() && home != real_home,
        "the command inherited the real home directory: {result}"
    );
    assert!(
        Path::new(&home).starts_with(std::fs::canonicalize(&workspace).unwrap()),
        "HOME escaped the Workspace: {home}"
    );
}

#[test]
fn an_empty_or_oversized_command_is_refused() {
    let workspace = workspace("bounds");

    let empty = invoke(workspace.path(), json!({"command": "   "}));
    assert_eq!(empty["is_error"], json!(true), "{empty}");

    let oversized = invoke(workspace.path(), json!({"command": "x".repeat(64 * 1024)}));
    assert_eq!(oversized["is_error"], json!(true), "{oversized}");
}

/// A command producing unbounded output must not be able to exhaust the
/// Worker's memory through the Tool result.
#[test]
fn command_output_is_truncated_rather_than_unbounded() {
    let workspace = workspace("output-bound");
    let result = invoke(
        workspace.path(),
        json!({"command": "yes abcdefghijklmnopqrstuvwxyz | head -c 400000"}),
    );

    assert_eq!(result["is_error"], json!(false), "{result}");
    let stdout = result["content"]["stdout"].as_str().unwrap();
    assert!(
        stdout.len() < 400_000,
        "stdout was not truncated: {} bytes",
        stdout.len()
    );
    assert_eq!(
        result["content"]["stdout_truncated"],
        json!(true),
        "{result}"
    );
}

#[test]
fn the_result_stays_bound_to_the_tool_call_that_asked_for_it() {
    let workspace = workspace("binding");
    let result = invoke(workspace.path(), json!({"command": "true"}));

    assert_eq!(result["tool_call_id"], json!("call_shell_1"), "{result}");
    assert_eq!(result["binding_digest"], json!("c".repeat(64)), "{result}");
}
