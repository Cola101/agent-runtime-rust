//! `workspace.write_text` behaviour, driven through the tool's real stdin/stdout
//! contract. Seatbelt is the outer containment boundary; these tests cover the
//! inner one, so a gap in either alone does not let a write escape.

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

fn workspace(label: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("agent-write-tool-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("workspace");
    path
}

/// Runs the tool exactly as the executor does: fixed argv, JSON on stdin, cwd
/// set to the Workspace.
fn invoke(workspace: &Path, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(tool_binary())
        .arg("--stdio")
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tool");
    let request = json!({
        "schema_version": 1,
        "tool_call": {"id": "call_write_1", "name": name, "arguments": arguments},
        "binding_digest": "a".repeat(64),
    });
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write request");
    let output = child.wait_with_output().expect("tool output");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "tool did not return JSON: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn writing_a_relative_path_creates_the_file_inside_the_workspace() {
    let workspace = workspace("happy");
    // The tool must not create missing parents implicitly; silently making
    // directories would let one write reshape the Workspace layout.
    let result = invoke(
        &workspace,
        "workspace.write_text",
        json!({"path": "notes/summary.md", "text": "evidence first"}),
    );
    assert_eq!(
        result["content"]["error"]["code"], "path_not_allowed",
        "a missing parent must be refused, not created: {result}"
    );

    std::fs::create_dir_all(workspace.join("notes")).expect("parent");
    let result = invoke(
        &workspace,
        "workspace.write_text",
        json!({"path": "notes/summary.md", "text": "evidence first"}),
    );
    assert_eq!(result["content"]["bytes"], 14, "unexpected: {result}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/summary.md")).unwrap(),
        "evidence first"
    );
}

#[test]
fn absolute_and_parent_relative_paths_are_refused() {
    let workspace = workspace("escape");
    for path in [
        "/tmp/escaped.txt",
        "../escaped.txt",
        "notes/../../escaped.txt",
    ] {
        let result = invoke(
            &workspace,
            "workspace.write_text",
            json!({"path": path, "text": "x"}),
        );
        assert_eq!(
            result["content"]["error"]["code"], "path_not_allowed",
            "path {path} was not refused: {result}"
        );
    }
}

#[test]
fn a_symlinked_path_component_is_refused_so_a_write_cannot_be_redirected() {
    let workspace = workspace("symlink");
    let outside = workspace.parent().unwrap().join("agent-write-tool-outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, workspace.join("link")).expect("symlink");

    let result = invoke(
        &workspace,
        "workspace.write_text",
        json!({"path": "link/escaped.txt", "text": "x"}),
    );
    assert_eq!(result["content"]["error"]["code"], "path_not_allowed");
    assert!(
        !outside.join("escaped.txt").exists(),
        "the write followed a symlink out of the workspace"
    );
}

#[test]
fn a_write_request_without_text_is_refused_before_touching_the_filesystem() {
    let workspace = workspace("missing-text");
    let mut child = Command::new(tool_binary())
        .arg("--stdio")
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let request = json!({
        "schema_version": 1,
        "tool_call": {"id": "call_write_1", "name": "workspace.write_text",
                      "arguments": {"path": "notes.md"}},
        "binding_digest": "a".repeat(64),
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "a malformed request must fail");
    assert!(!workspace.join("notes.md").exists());
}
