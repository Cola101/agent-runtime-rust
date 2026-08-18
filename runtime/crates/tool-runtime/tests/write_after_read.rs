//! A write that follows a read in the same Run carries what that read saw.
//!
//! The trusted tool refuses a write whose `expected_sha256` no longer matches
//! (`file_changed_since_read`), and nothing sent that field -- so the check
//! existed and never fired. This covers the half that makes it fire: the
//! executor remembers, per Run, what each read returned, and hands it back on
//! the write.
//!
//! Driven through a stand-in tool rather than the real one, because what is
//! under test is what the executor *sends*, not what the tool does with it.

use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ToolExecutionContext, TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn temporary(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("agent-write-after-read-{label}-"))
        .tempdir()
        .expect("temp dir")
}

/// A tool that answers a read with fixed text and records every request it was
/// given, so the test can read back what the executor sent.
fn recording_tool(root: &Path, workspace: &Path, text: &str) -> PathBuf {
    let executable = root.join("recording-tool");
    // In the workspace, not the trusted root: containment makes the trusted
    // root read-only to the tool, which is containment working rather than a
    // problem to route around.
    let log = workspace.join("requests.jsonl");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nset -eu\nrequest=$(cat)\nprintf '%s\\n' \"$request\" >> {log}\n\
             id=$(printf '%s' \"$request\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p')\n\
             digest=$(printf '%s' \"$request\" | sed -n 's/.*\"binding_digest\":\"\\([^\"]*\\)\".*/\\1/p')\n\
             case \"$request\" in\n\
               *write_text*) body='{{\"path\":\"notes.txt\",\"text\":\"written\",\"bytes\":7}}' ;;\n\
               *) body='{{\"path\":\"notes.txt\",\"text\":\"{text}\",\"bytes\":4}}' ;;\n\
             esac\n\
             printf '{{\"tool_call_id\":\"%s\",\"binding_digest\":\"%s\",\"content\":%s,\"is_error\":false}}' \\\n\
               \"$id\" \"$digest\" \"$body\"\n",
            log = log.display(),
            text = text,
        ),
    )
    .expect("write tool");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("chmod");
    executable
}

fn requests(workspace: &Path) -> Vec<Value> {
    fs::read_to_string(workspace.join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each request is one JSON object"))
        .collect()
}

fn context(run: Uuid, workspace: &Path) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::nil(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: run,
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        workspace_root: workspace.to_path_buf(),
        timeout: Duration::from_secs(10),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

fn call(name: &str, arguments: Value) -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall { id: format!("call_{name}"), name: name.into(), arguments },
        effect: if name == "workspace.write_text" { ToolEffect::NonIdempotent } else { ToolEffect::Pure },
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "b".repeat(64),
    }
}

/// sha256 of the text the read returned, which is what the write must carry.
fn sha256_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    format!("{:x}", digest.finalize())
}

#[tokio::test]
async fn a_write_after_a_read_carries_what_that_read_saw() {
    let trusted = temporary("trusted");
    let workspace = temporary("workspace");
    let executable = recording_tool(trusted.path(), workspace.path(), "one");
    let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: trusted.path().to_path_buf(),
        executable,
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    })
    .expect("executor");
    let run = Uuid::now_v7();

    executor
        .execute(call("workspace.read_text", json!({ "path": "notes.txt" })), context(run, workspace.path()))
        .await
        .expect("read");
    executor
        .execute(
            call("workspace.write_text", json!({ "path": "notes.txt", "text": "two\n" })),
            context(run, workspace.path()),
        )
        .await
        .expect("write");

    let sent = requests(workspace.path());
    let write = sent.iter().find(|r| r["tool_call_id"] == "call_workspace.write_text")
        .or_else(|| sent.iter().find(|r| r["tool_call"]["name"] == "workspace.write_text"))
        .expect("the write request was recorded");
    assert_eq!(
        write["tool_call"]["arguments"]["expected_sha256"], sha256_of("one"),
        "the write must carry the digest of what this Run read: {write}",
    );
}

/// A path this Run never read has no expectation to carry. Creating a file, or
/// deliberately replacing one nobody looked at, must keep working.
#[tokio::test]
async fn a_write_with_no_earlier_read_carries_no_expectation() {
    let trusted = temporary("trusted-fresh");
    let workspace = temporary("workspace-fresh");
    let executable = recording_tool(trusted.path(), workspace.path(), "one");
    let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: trusted.path().to_path_buf(),
        executable,
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    })
    .expect("executor");

    executor
        .execute(
            call("workspace.write_text", json!({ "path": "fresh.txt", "text": "hello\n" })),
            context(Uuid::now_v7(), workspace.path()),
        )
        .await
        .expect("write");

    let sent = requests(workspace.path());
    let write = sent.first().expect("one request");
    assert!(
        write["tool_call"]["arguments"]["expected_sha256"].is_null(),
        "a write to a path nobody read must not claim an expectation: {write}",
    );
}

/// Two Runs are two accounts. What one Run read says nothing about what another
/// Run is allowed to overwrite, and mixing them would refuse writes for a
/// reason that has nothing to do with the Run being refused.
#[tokio::test]
async fn one_runs_reads_do_not_constrain_another_runs_writes() {
    let trusted = temporary("trusted-two");
    let workspace = temporary("workspace-two");
    let executable = recording_tool(trusted.path(), workspace.path(), "one");
    let executor = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: trusted.path().to_path_buf(),
        executable,
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    })
    .expect("executor");

    executor
        .execute(
            call("workspace.read_text", json!({ "path": "notes.txt" })),
            context(Uuid::now_v7(), workspace.path()),
        )
        .await
        .expect("read in the first Run");
    executor
        .execute(
            call("workspace.write_text", json!({ "path": "notes.txt", "text": "two\n" })),
            context(Uuid::now_v7(), workspace.path()),
        )
        .await
        .expect("write in a second Run");

    let sent = requests(workspace.path());
    let write = sent.iter().find(|r| r["tool_call"]["name"] == "workspace.write_text")
        .expect("the write was recorded");
    assert!(
        write["tool_call"]["arguments"]["expected_sha256"].is_null(),
        "another Run's read must not become this Run's expectation: {write}",
    );
}

/// The whole chain, with the real tool and a real file: a Run reads, someone
/// edits the file while the approval is on screen, and the Run's write is
/// refused with the hand edit intact.
///
/// The two tests above cover the halves -- the executor sends what it saw, the
/// tool refuses a mismatch -- and halves passing is not the same as the thing
/// working. This is the one that would have caught the clobber.
#[tokio::test]
async fn a_write_is_refused_when_the_file_changed_while_the_approval_was_on_screen() {
    let workspace = temporary("end-to-end");
    fs::write(workspace.path().join("notes.txt"), "one\n").expect("seed");

    // The real binary, found the way its own tests find it.
    let executor = real_executor();
    let run = Uuid::now_v7();

    let read = executor
        .execute(
            call("workspace.read_text", json!({ "path": "notes.txt" })),
            context(run, workspace.path()),
        )
        .await
        .expect("read");
    assert_eq!(read.content["text"], "one\n");

    // The edit that arrives while a person is deciding.
    fs::write(workspace.path().join("notes.txt"), "one\nadded by hand\n").expect("edit");

    let wrote = executor
        .execute(
            call("workspace.write_text", json!({ "path": "notes.txt", "text": "rewritten\n" })),
            context(run, workspace.path()),
        )
        .await
        .expect("the call itself completes; the refusal is in the result");

    assert!(wrote.is_error, "a write over a changed file must be refused: {:?}", wrote.content);
    assert_eq!(wrote.content["error"]["code"], "file_changed_since_read");
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).expect("read back"),
        "one\nadded by hand\n",
        "the edit made while the approval was on screen must survive",
    );
}

/// And the same Run writing twice is not a conflict with itself.
#[tokio::test]
async fn a_run_that_writes_twice_is_not_refused_for_its_own_first_write() {
    let workspace = temporary("twice");
    fs::write(workspace.path().join("notes.txt"), "one\n").expect("seed");

    let executor = real_executor();
    let run = Uuid::now_v7();

    executor
        .execute(
            call("workspace.read_text", json!({ "path": "notes.txt" })),
            context(run, workspace.path()),
        )
        .await
        .expect("read");
    let first = executor
        .execute(
            call("workspace.write_text", json!({ "path": "notes.txt", "text": "second\n" })),
            context(run, workspace.path()),
        )
        .await
        .expect("first write");
    assert!(!first.is_error, "{:?}", first.content);

    let second = executor
        .execute(
            call("workspace.write_text", json!({ "path": "notes.txt", "text": "third\n" })),
            context(run, workspace.path()),
        )
        .await
        .expect("second write");
    assert!(
        !second.is_error,
        "a Run must not be refused for the change it made itself: {:?}",
        second.content,
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).expect("read back"),
        "third\n",
    );
}

/// The shipped tool binary, found the way its own tests find it.
fn real_executor() -> TrustedNativeExecutor {
    let mut current = std::env::current_exe().expect("test binary path");
    let tool = loop {
        if !current.pop() {
            panic!("agent-trusted-workspace-tool must be built");
        }
        let candidate = current.join("agent-trusted-workspace-tool");
        if candidate.is_file() {
            break candidate;
        }
    };
    let trusted_root = tool.parent().expect("the tool has a parent").to_path_buf();
    TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root,
        executable: tool,
        fixed_args: vec!["--stdio".into()],
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 8 * 1024,
    })
    .expect("executor")
}

/// A model that puts `expected_sha256` in its own arguments does not get to
/// keep it.
///
/// It could not widen anything -- the check only refuses -- but it could make
/// its own writes fail for a reason nobody on the outside could see. The field
/// belongs to the executor in both directions: dropped when there is nothing to
/// add, replaced when there is.
#[tokio::test]
async fn a_digest_the_model_supplied_is_not_the_one_that_is_checked() {
    let workspace = temporary("model-supplied");
    fs::write(workspace.path().join("notes.txt"), "one\n").expect("seed");
    let executor = real_executor();
    let run = Uuid::now_v7();

    // Never read by this Run, and the model invents an expectation anyway.
    let wrote = executor
        .execute(
            call(
                "workspace.write_text",
                json!({
                    "path": "notes.txt",
                    "text": "rewritten\n",
                    "expected_sha256": "0".repeat(64),
                }),
            ),
            context(run, workspace.path()),
        )
        .await
        .expect("the call completes");
    assert!(
        !wrote.is_error,
        "a digest the model made up must not decide anything: {:?}",
        wrote.content,
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("notes.txt")).expect("read back"),
        "rewritten\n",
    );
}
