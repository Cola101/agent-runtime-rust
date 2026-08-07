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

fn workspace() -> PathBuf {
    let path = std::env::temp_dir().join(format!("agent-tool-runtime-{}", Uuid::now_v7()));
    fs::create_dir_all(&path).unwrap();
    path
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
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        workspace_root,
        timeout: Duration::from_secs(30),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

fn executable_script(body: &str) -> PathBuf {
    let engine = workspace().join("container-engine.sh");
    fs::write(&engine, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&engine).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
        fs::set_permissions(&engine, permissions).unwrap();
    }
    engine
}

#[test]
fn restricted_container_is_digest_pinned_non_networked_and_argument_safe() {
    let workspace_root = workspace();
    let canonical_workspace = fs::canonicalize(&workspace_root).unwrap();
    let executor = RestrictedContainerExecutor::new("/usr/local/bin/docker", definition()).unwrap();

    let launch = executor
        .prepare(&request(), &context(workspace_root.clone()))
        .unwrap();

    assert_eq!(launch.program, "/usr/local/bin/docker");
    assert_eq!(launch.stdin_json["tool_call"]["id"], "call_http_7");
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
    let mut kata = request();
    kata.sandbox = SandboxClass::Kata;
    assert_eq!(
        executor.prepare(&kata, &context(workspace())).unwrap_err(),
        ToolExecutionError::WrongSandbox
    );
}

#[tokio::test]
async fn process_timeout_is_an_error_not_a_successful_empty_tool_result() {
    let engine = executable_script("sleep 5");
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();
    let mut execution_context = context(workspace());
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
    let engine = executable_script(
        "cat >/dev/null\nprintf '%s' '{\"tool_call_id\":\"call_http_7\",\"binding_digest\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",\"content\":{\"ok\":true},\"is_error\":false}'",
    );
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();

    assert_eq!(
        executor
            .execute(request(), context(workspace()))
            .await
            .unwrap_err(),
        ToolExecutionError::OutputBindingMismatch
    );
}

#[tokio::test]
async fn bound_container_result_is_returned_without_losing_tool_error_semantics() {
    let engine = executable_script(
        "cat >/dev/null\nprintf '%s' '{\"tool_call_id\":\"call_http_7\",\"binding_digest\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"content\":{\"status\":403},\"is_error\":true}'",
    );
    let executor =
        RestrictedContainerExecutor::new(engine.to_string_lossy(), definition()).unwrap();

    let result = executor
        .execute(request(), context(workspace()))
        .await
        .unwrap();

    assert_eq!(result.content, json!({"status":403}));
    assert!(result.is_error);
    assert_eq!(result.exit_code, 0);
}
