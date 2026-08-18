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

/// Returns a guard, not a path: dropping it removes the directory, so a run
/// of this suite leaves nothing behind.
fn workspace(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("agent-write-tool-{label}-"))
        .tempdir()
        .expect("workspace")
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
        workspace.path(),
        "workspace.write_text",
        json!({"path": "notes/summary.md", "text": "evidence first"}),
    );
    assert_eq!(
        result["content"]["error"]["code"], "path_not_allowed",
        "a missing parent must be refused, not created: {result}"
    );

    std::fs::create_dir_all(workspace.path().join("notes")).expect("parent");
    let result = invoke(
        workspace.path(),
        "workspace.write_text",
        json!({"path": "notes/summary.md", "text": "evidence first"}),
    );
    assert_eq!(result["content"]["bytes"], 14, "unexpected: {result}");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes/summary.md")).unwrap(),
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
            workspace.path(),
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
    // Its own guard: the escape target must outlive the assertion below.
    let outside_guard = tempfile::Builder::new()
        .prefix("agent-write-tool-outside-")
        .tempdir()
        .expect("outside dir");
    let outside = outside_guard.path().to_path_buf();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, workspace.path().join("link")).expect("symlink");

    let result = invoke(
        workspace.path(),
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
    assert!(!workspace.path().join("notes.md").exists());
}

/// A write replaces whatever is there, including something that arrived after
/// the agent decided what to write.
///
/// This is the sequence a desktop session actually produces: the agent reads a
/// file, composes new contents, the person is shown what would change and
/// approves -- and in the gap between the read and the write, an editor or
/// another Run touches the same file. The write lands on top of it and nothing
/// anywhere records that anything was lost.
///
/// `docs/desktop-ui-gap.md` has said 不得静默覆盖 about this since before this
/// test existed, and the approval card added this week draws a comparison
/// computed *before* that gap, which makes approving feel better grounded than
/// it is.
///
/// Marked `#[ignore]` rather than left failing: it is a statement of what is
/// wanted, not a regression, and turning the suite red would say a change
/// broke something. Run it with `--ignored` to see the clobber.
#[test]
#[ignore = "records the silent clobber; the fix is a contract decision, see docs/evidence/2026-08-19-write-clobbers-silently.md"]
fn a_write_refuses_to_replace_a_file_that_changed_after_it_was_read() {
    let workspace = workspace("clobber");
    let path = workspace.path().join("notes.txt");
    std::fs::write(&path, "one\n").expect("seed the file");

    // What the agent read, and what it decided to write from it.
    let read = invoke(workspace.path(), "workspace.read_text", json!({ "path": "notes.txt" }));
    assert_eq!(read["content"]["text"], "one\n");

    // Someone else edits the same file while the approval is on screen.
    std::fs::write(&path, "one\ntwo, added by hand\n").expect("edit under it");

    let wrote = invoke(
        workspace.path(),
        "workspace.write_text",
        json!({ "path": "notes.txt", "text": "one\nrewritten by the agent\n" }),
    );

    // What is wanted: a refusal naming the mismatch, and the hand edit intact.
    assert_eq!(wrote["is_error"], true, "a write over a changed file must be refused");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "one\ntwo, added by hand\n",
        "the edit made while the approval was on screen must survive",
    );
}
