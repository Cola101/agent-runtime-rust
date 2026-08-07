use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn invoke(workspace: &Path, arguments: Value) -> (std::process::ExitStatus, Value, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-trusted-workspace-tool"))
        .arg("--stdio")
        .current_dir(workspace)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = json!({
        "schema_version": 1,
        "tenant_id": "11111111-1111-4111-8111-111111111111",
        "run_id": "22222222-2222-4222-8222-222222222222",
        "attempt_id": "33333333-3333-4333-8333-333333333333",
        "requested_at": "2026-08-02T00:00:00Z",
        "timeout_ms": 5000,
        "tool_call": {
            "id": "call_read_1",
            "name": "workspace.read_text",
            "arguments": arguments
        },
        "binding_digest": "b".repeat(64)
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&request).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let response = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (
        output.status,
        response,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn reads_a_bounded_utf8_file_and_binds_the_result_to_the_reviewed_call() {
    let root = workspace();
    fs::write(root.path().join("README.txt"), "native tool result\n").unwrap();

    let (status, response, stderr) = invoke(root.path(), json!({"path":"README.txt"}));

    assert!(status.success(), "{stderr}");
    assert_eq!(response["tool_call_id"], "call_read_1");
    assert_eq!(response["binding_digest"], "b".repeat(64));
    assert_eq!(response["content"]["path"], "README.txt");
    assert_eq!(response["content"]["text"], "native tool result\n");
    assert_eq!(response["content"]["bytes"], 19);
    assert_eq!(response["is_error"], false);
}

#[test]
fn traversal_and_symlinks_are_denied_without_disclosing_outside_content() {
    let root = workspace();
    let outside_root = workspace();
    let outside = outside_root.path().join("outside.txt");
    fs::write(&outside, "must-not-leak").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.path().join("link.txt")).unwrap();

    for path in ["../outside.txt", "link.txt"] {
        let (status, response, stderr) = invoke(root.path(), json!({"path":path}));
        assert!(status.success(), "{stderr}");
        assert_eq!(response["is_error"], true);
        assert_eq!(response["content"]["error"]["code"], "path_not_allowed");
        assert!(!response.to_string().contains("must-not-leak"));
    }
}

#[test]
fn oversized_or_non_utf8_files_return_bounded_structured_errors() {
    let root = workspace();
    fs::write(root.path().join("large.txt"), vec![b'x'; 65_537]).unwrap();
    fs::write(root.path().join("binary.txt"), [0xff, 0xfe]).unwrap();

    let (_, large, _) = invoke(root.path(), json!({"path":"large.txt"}));
    let (_, binary, _) = invoke(root.path(), json!({"path":"binary.txt"}));

    assert_eq!(large["content"]["error"]["code"], "file_too_large");
    assert_eq!(binary["content"]["error"]["code"], "not_utf8");
}
