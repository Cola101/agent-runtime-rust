use agent_protocol::{SandboxClass, ToolCall, ToolEffect, ToolExecutionRequest};
use agent_tool_runtime::{
    ToolExecutionContext, ToolExecutionError, TrustedNativeExecutor, TrustedNativeToolDefinition,
    WorkspaceAccess,
};
use chrono::Utc;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn temporary_directory(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agent-native-tool-{label}-{}", Uuid::now_v7()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn executable_script(root: &Path, body: &str) -> PathBuf {
    let executable = root.join("trusted-tool");
    fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn definition(trusted_root: &Path, executable: &Path) -> TrustedNativeToolDefinition {
    TrustedNativeToolDefinition {
        trusted_root: trusted_root.to_path_buf(),
        executable: executable.to_path_buf(),
        fixed_args: vec!["--stdio".into()],
        workspace_access: WorkspaceAccess::ReadOnly,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 16 * 1024,
    }
}

fn request() -> ToolExecutionRequest {
    ToolExecutionRequest {
        call: ToolCall {
            id: "call_workspace_read_1".into(),
            name: "workspace.read_text".into(),
            arguments: json!({"path":"README.txt"}),
        },
        effect: ToolEffect::Pure,
        sandbox: SandboxClass::TrustedNative,
        binding_digest: "b".repeat(64),
    }
}

fn context(workspace_root: PathBuf) -> ToolExecutionContext {
    ToolExecutionContext {
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        workspace_root,
        timeout: Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        requested_at: Utc::now(),
    }
}

#[test]
fn trusted_native_launch_is_root_bound_and_never_places_model_arguments_in_argv_or_env() {
    let trusted_root = temporary_directory("root-bound");
    let executable = executable_script(&trusted_root, "exit 0");
    let workspace = temporary_directory("workspace");
    let executor = TrustedNativeExecutor::new(definition(&trusted_root, &executable)).unwrap();

    let launch = executor
        .prepare(&request(), &context(workspace.clone()))
        .unwrap();

    // The launch is now wrapped by Seatbelt on macOS, so the registered
    // executable moves from `program` into argv after the `--` separator. Both
    // original invariants still have to hold: exactly the registered executable
    // runs, and its fixed arguments follow it unchanged.
    let canonical = fs::canonicalize(&executable).unwrap();
    #[cfg(target_os = "macos")]
    {
        assert_eq!(launch.program, PathBuf::from("/usr/bin/sandbox-exec"));
        let separator = launch
            .args
            .iter()
            .position(|argument| argument == "--")
            .expect("the wrapped launch must separate sandbox arguments from the tool");
        assert_eq!(
            launch.args[separator + 1],
            canonical.display().to_string(),
            "the contained process must be exactly the registered executable"
        );
        assert_eq!(launch.args[separator + 2..], ["--stdio".to_string()]);
        assert!(
            launch.args[..separator]
                .iter()
                .any(|argument| argument.starts_with("(version 1)\n(deny default)")),
            "the sandbox profile must be closed by default"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert_eq!(launch.program, canonical);
        assert_eq!(launch.args, vec!["--stdio"]);
    }
    assert!(launch.env.is_empty());
    assert_eq!(launch.current_dir, fs::canonicalize(workspace).unwrap());
    assert_eq!(
        launch.stdin_json["tool_call"]["id"],
        "call_workspace_read_1"
    );
    assert_eq!(
        launch.stdin_json["tool_call"]["arguments"]["path"],
        "README.txt"
    );
    assert!(
        !launch
            .args
            .iter()
            .any(|argument| argument.contains("README.txt"))
    );
}

#[test]
fn executable_outside_trusted_root_and_wrong_sandbox_fail_closed() {
    let trusted_root = temporary_directory("trusted-root");
    let outside_root = temporary_directory("outside-root");
    let outside = executable_script(&outside_root, "exit 0");
    assert!(matches!(
        TrustedNativeExecutor::new(definition(&trusted_root, &outside)),
        Err(ToolExecutionError::InvalidDefinition(_))
    ));

    let executable = executable_script(&trusted_root, "exit 0");
    let executor = TrustedNativeExecutor::new(definition(&trusted_root, &executable)).unwrap();
    let mut wrong_sandbox = request();
    wrong_sandbox.sandbox = SandboxClass::RestrictedContainer;
    assert_eq!(
        executor
            .prepare(
                &wrong_sandbox,
                &context(temporary_directory("wrong-sandbox"))
            )
            .unwrap_err(),
        ToolExecutionError::WrongSandbox
    );
}

#[test]
fn executable_replacement_after_registration_is_rejected_before_spawn() {
    let trusted_root = temporary_directory("drift");
    let executable = executable_script(&trusted_root, "exit 0");
    let executor = TrustedNativeExecutor::new(definition(&trusted_root, &executable)).unwrap();
    fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();

    assert_eq!(
        executor
            .prepare(&request(), &context(temporary_directory("drift-workspace")))
            .unwrap_err(),
        ToolExecutionError::ExecutableChanged
    );
}

#[test]
fn implementation_digest_binds_the_executable_bytes_and_fixed_arguments() {
    let trusted_root = temporary_directory("implementation-digest");
    let executable = executable_script(&trusted_root, "exit 0");
    let first = TrustedNativeExecutor::new(definition(&trusted_root, &executable)).unwrap();
    let mut changed_definition = definition(&trusted_root, &executable);
    changed_definition.fixed_args = vec!["--different-mode".into()];
    let changed = TrustedNativeExecutor::new(changed_definition).unwrap();

    assert_eq!(first.implementation_digest().len(), 64);
    assert_ne!(
        first.implementation_digest(),
        changed.implementation_digest()
    );
}

#[tokio::test]
async fn bound_native_result_is_returned_and_preserves_tool_error_semantics() {
    let trusted_root = temporary_directory("execute");
    let executable = executable_script(
        &trusted_root,
        "/bin/cat >/dev/null\nprintf '%s' '{\"tool_call_id\":\"call_workspace_read_1\",\"binding_digest\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",\"content\":{\"text\":\"hello\"},\"is_error\":false}'",
    );
    let executor = TrustedNativeExecutor::new(definition(&trusted_root, &executable)).unwrap();

    let result = executor
        .execute(request(), context(temporary_directory("execute-workspace")))
        .await
        .unwrap();

    assert_eq!(result.content, json!({"text":"hello"}));
    assert!(!result.is_error);
    assert_eq!(result.exit_code, 0);
}

/// A trusted Tool is trusted to be the binary we registered, not trusted to be
/// free of bugs. Containment is what stops a defect or a crafted argument from
/// touching anything outside the Workspace.
#[tokio::test]
async fn a_trusted_native_tool_cannot_write_outside_its_workspace() {
    let root = temporary_directory("contain-write");
    let workspace = temporary_directory("contain-write-ws");
    let escape = temporary_directory("contain-write-escape").join("escaped.txt");
    let executable = executable_script(
        &root,
        &format!(
            "cat > /dev/null\n/usr/bin/touch '{}' 2>/dev/null || true\nprintf '{{\"content\":{{}},\"is_error\":false}}'",
            escape.display()
        ),
    );

    let executor = TrustedNativeExecutor::new(definition(&root, &executable)).unwrap();
    let _ = executor
        .execute(
            request(),
            ToolExecutionContext {
                tenant_id: Uuid::now_v7(),
                run_id: Uuid::now_v7(),
                attempt_id: Uuid::now_v7(),
                workspace_root: workspace.clone(),
                timeout: Duration::from_secs(10),
                cancellation: CancellationToken::new(),
                requested_at: Utc::now(),
            },
        )
        .await;

    assert!(
        !escape.exists(),
        "the tool wrote outside its workspace at {}",
        escape.display()
    );
}

/// The trusted Tools this platform ships declare no network capability
/// (ADR-0025). Containment must make that structural rather than a promise.
#[tokio::test]
async fn a_trusted_native_tool_cannot_reach_the_network() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            drop(stream);
        }
    });

    let root = temporary_directory("contain-net");
    let workspace = temporary_directory("contain-net-ws");
    let marker = workspace.join("reached-network");
    let executable = executable_script(
        &root,
        &format!(
            "cat > /dev/null\nif /usr/bin/nc -z -G 2 127.0.0.1 {port} 2>/dev/null; then /usr/bin/touch '{}' 2>/dev/null || true; fi\nprintf '{{\"content\":{{}},\"is_error\":false}}'",
            marker.display()
        ),
    );

    let executor = TrustedNativeExecutor::new(definition(&root, &executable)).unwrap();
    let _ = executor
        .execute(
            request(),
            ToolExecutionContext {
                tenant_id: Uuid::now_v7(),
                run_id: Uuid::now_v7(),
                attempt_id: Uuid::now_v7(),
                workspace_root: workspace.clone(),
                timeout: Duration::from_secs(10),
                cancellation: CancellationToken::new(),
                requested_at: Utc::now(),
            },
        )
        .await;

    assert!(
        !marker.exists(),
        "the tool opened an outbound connection from inside the sandbox"
    );
}
