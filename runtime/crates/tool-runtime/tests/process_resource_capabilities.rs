use agent_tool_runtime::{
    PersistentProcessSessionManager, ProcessSessionError, ProcessSessionGovernance,
    ProcessSessionResourceBackendConfig, ProcessSessionResourceBackendKind,
    ProcessSessionResourceCapabilities, TrustedNativeExecutor, TrustedNativeToolDefinition,
    WorkspaceAccess,
};
use std::fs;
use std::path::{Path, PathBuf};

fn executable_script(root: &Path) -> PathBuf {
    let executable = root.join("resource-capability-session");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    executable
}

fn executor(root: &Path, executable: &Path) -> TrustedNativeExecutor {
    TrustedNativeExecutor::new(TrustedNativeToolDefinition {
        trusted_root: root.to_path_buf(),
        executable: executable.to_path_buf(),
        fixed_args: Vec::new(),
        workspace_access: WorkspaceAccess::ReadWrite,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 16 * 1024,
    })
    .unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn macos_capabilities_do_not_claim_memory_process_or_tree_accounting() {
    assert_eq!(
        ProcessSessionResourceCapabilities::current(),
        ProcessSessionResourceCapabilities {
            backend: ProcessSessionResourceBackendKind::UnixRlimit,
            hard_output_file_limit: true,
            hard_cpu_time_limit: true,
            hard_memory_limit: false,
            hard_process_count_limit: false,
            whole_process_tree_accounting: false,
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_memory_requirement_fails_with_the_missing_capability() {
    let root = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let state_root = root.path().join("not-created");
    let governance = ProcessSessionGovernance {
        max_memory_bytes: Some(256 * 1024 * 1024),
        ..ProcessSessionGovernance::default()
    };

    let error = match PersistentProcessSessionManager::new_with_governance(
        state_root.clone(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    ) {
        Ok(_) => panic!("an unenforceable memory limit was accepted"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ProcessSessionError::UnsupportedResourceCapability("hard_memory_limit")
    ));
    assert!(!state_root.exists());
}

#[test]
fn unsupported_process_count_limit_is_rejected_before_state_creation() {
    let root = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let state_root = root.path().join("not-created");
    let governance = ProcessSessionGovernance {
        max_processes: Some(8),
        ..ProcessSessionGovernance::default()
    };

    let error = match PersistentProcessSessionManager::new_with_governance(
        state_root.clone(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    ) {
        Ok(_) => panic!("an unenforceable process count limit was accepted"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ProcessSessionError::UnsupportedResourceCapability("hard_process_count_limit")
    ));
    assert!(!state_root.exists());
}

#[test]
fn unsupported_whole_tree_accounting_is_rejected_before_state_creation() {
    let root = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let state_root = root.path().join("not-created");
    let governance = ProcessSessionGovernance {
        require_whole_process_tree_accounting: true,
        ..ProcessSessionGovernance::default()
    };

    let error = match PersistentProcessSessionManager::new_with_governance(
        state_root.clone(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    ) {
        Ok(_) => panic!("unenforceable whole-tree accounting was accepted"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ProcessSessionError::UnsupportedResourceCapability("whole_process_tree_accounting")
    ));
    assert!(!state_root.exists());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_cgroup_backend_is_rejected_before_state_creation_on_this_platform() {
    let root = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let executable = executable_script(trusted.path());
    let state_root = root.path().join("not-created");
    let governance = ProcessSessionGovernance {
        resource_backend: ProcessSessionResourceBackendConfig::LinuxCgroupV2 {
            delegated_root: root.path().join("delegated-cgroup"),
        },
        max_processes: Some(8),
        require_whole_process_tree_accounting: true,
        ..ProcessSessionGovernance::default()
    };

    let error = match PersistentProcessSessionManager::new_with_governance(
        state_root.clone(),
        executor(trusted.path(), &executable),
        16 * 1024,
        governance,
    ) {
        Ok(_) => panic!("Linux cgroup backend was accepted on a non-Linux host"),
        Err(error) => error,
    };

    assert_eq!(error, ProcessSessionError::UnsupportedPlatform);
    assert!(!state_root.exists());
}
