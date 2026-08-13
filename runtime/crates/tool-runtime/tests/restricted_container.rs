use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ContainerToolDefinition, RestrictedContainerExecutor, ToolExecutionContext, ToolExecutionError,
    WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Returns a guard, not a path: dropping it removes the directory. Callers must
/// bind it for as long as the directory is needed.
fn workspace() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agent-tool-runtime-")
        .tempdir()
        .unwrap()
}

fn definition() -> ContainerToolDefinition {
    ContainerToolDefinition {
        image: "registry.example.com/runtime/http-tool@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        entrypoint: vec!["/opt/tool/bin/http-tool".into()],
        workspace_access: WorkspaceAccess::ReadOnly,
        memory_bytes: 256 * 1024 * 1024,
        cpu_millis: 500,
        pids_limit: 64,
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 64 * 1024,
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_http_7".into(),
            name: "http".into(),
            arguments: json!({"url":"https://example.com/a?token=secret"}),
        },
        effect: ToolEffect::Idempotent,
        sandbox: SandboxClass::RestrictedContainer,
        binding_digest: "b".repeat(64),
    }
}

fn context(workspace_root: PathBuf) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::now_v7(),
        workload_identity_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        agent_version_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        workspace_root,
        timeout: Duration::from_secs(30),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

fn executable_script(body: &str) -> (tempfile::TempDir, PathBuf) {
    let root = workspace();
    let engine = root.path().join("container-engine.sh");
    fs::write(&engine, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&engine).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
        fs::set_permissions(&engine, permissions).unwrap();
    }
    (root, engine)
}

#[test]
fn restricted_container_is_digest_pinned_non_networked_and_argument_safe() {
    let workspace_root = workspace();
    let canonical_workspace = fs::canonicalize(workspace_root.path()).unwrap();
    let executor = RestrictedContainerExecutor::new("/usr/local/bin/docker", definition()).unwrap();

    let launch = executor
        .prepare(&request(), &context(workspace_root.path().to_path_buf()))
        .unwrap();

    assert_eq!(launch.program, "/usr/local/bin/docker");
    assert_eq!(launch.stdin_json["tool_call"]["id"], "call_http_7");
    assert!(launch.stdin_json["application_id"].is_string());
    assert!(launch.stdin_json["workload_identity_id"].is_string());
    assert!(launch.stdin_json["session_id"].is_string());
    assert!(launch.stdin_json["workspace_id"].is_string());
    assert!(launch.stdin_json["agent_version_id"].is_string());
    assert_eq!(
        launch.stdin_json["tool_call"]["arguments"]["url"],
        "https://example.com/a?token=secret"
    );
    assert!(
        launch
            .args
            .windows(2)
            .any(|args| args == ["--network", "none"])
    );
    assert!(launch.args.iter().any(|arg| arg == "--read-only"));
    assert!(
        launch
            .args
            .windows(2)
            .any(|args| args == ["--cap-drop", "ALL"])
    );
    assert!(
        launch
            .args
            .windows(2)
            .any(|args| args == ["--security-opt", "no-new-privileges"])
    );
    assert!(launch.args.windows(2).any(|args| {
        args[0] == "--mount"
            && args[1]
                == format!(
                    "type=bind,src={},dst=/workspace,readonly",
                    canonical_workspace.display()
                )
    }));
    assert!(launch.args.iter().any(|arg| arg.contains("@sha256:")));
    assert!(!launch.args.iter().any(|arg| arg.contains("token=secret")));
    assert!(launch.env.is_empty());
}

#[test]
fn floating_image_and_non_restricted_request_are_rejected_before_spawn() {
    let mut floating = definition();
    floating.image = "registry.example.com/runtime/http-tool:latest".into();
    assert!(matches!(
        RestrictedContainerExecutor::new("/usr/local/bin/docker", floating),
        Err(ToolExecutionError::InvalidDefinition(_))
    ));

    assert!(matches!(
        RestrictedContainerExecutor::new("docker", definition()),
        Err(ToolExecutionError::InvalidDefinition(_))
    ));

    let executor = RestrictedContainerExecutor::new("/usr/local/bin/docker", definition()).unwrap();
    let scratch = workspace();
    let mut kata = request();
    kata.sandbox = SandboxClass::Kata;
    assert_eq!(
        executor
            .prepare(&kata, &context(scratch.path().to_path_buf()))
            .unwrap_err(),
        ToolExecutionError::WrongSandbox
    );
}

#[tokio::test]
async fn process_timeout_is_an_error_not_a_successful_empty_tool_result() {
    let (_engine_root, engine) = executable_script("sleep 5");
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();
    let scratch = workspace();
    let mut execution_context = context(scratch.path().to_path_buf());
    execution_context.timeout = Duration::from_millis(50);

    assert_eq!(
        executor
            .execute(request(), execution_context)
            .await
            .unwrap_err(),
        ToolExecutionError::TimedOut
    );
}

#[tokio::test]
async fn container_result_for_another_tool_call_is_rejected() {
    let (_engine_root, engine) = executable_script(
        "cat >/dev/null\nprintf '%s' '{\"tool_call_id\":\"call_http_7\",\"binding_digest\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",\"content\":{\"ok\":true},\"is_error\":false}'",
    );
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();
    let scratch = workspace();

    assert_eq!(
        executor
            .execute(request(), context(scratch.path().to_path_buf()))
            .await
            .unwrap_err(),
        ToolExecutionError::OutputBindingMismatch
    );
}

#[tokio::test]
async fn bound_container_result_is_returned_without_losing_tool_error_semantics() {
    let (_engine_root, engine) = executable_script(
        "cat >/dev/null\nprintf '%s' '{\"tool_call_id\":\"call_http_7\",\"binding_digest\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"content\":{\"status\":403},\"is_error\":true}'",
    );
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();
    let scratch = workspace();

    let result = executor
        .execute(request(), context(scratch.path().to_path_buf()))
        .await
        .unwrap();

    assert_eq!(result.content, json!({"status":403}));
    assert!(result.is_error);
    assert_eq!(result.exit_code, 0);
}
