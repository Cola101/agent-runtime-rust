//! The deployed binary must not be able to serve the invocation surface in the
//! clear.
//!
//! These spawn the real `runtime-host` binary rather than calling a library
//! function, because the property under test is about what a deployment does,
//! not about what a helper returns. `load_invocation_surface` lives in
//! `main.rs` and is unreachable from a library test -- which is the right place
//! for it, and this is the way to prove it.
//!
//! The local Unix socket could treat "you can open this file" as the
//! authorization. A TCP port has no equivalent, and a surface that starts Runs
//! and spends a tenant's budget is not where that should be discovered.

use std::process::Command;

/// Runs `runtime-host serve` with a deliberately incomplete environment and
/// returns its stderr.
///
/// Every variable the surface needs is absent unless named in `extra`, and the
/// state root is a path that does not exist -- so if the surface configuration
/// were ever skipped, the process would still fail, just with a different
/// message. The assertions therefore check *which* failure happened, never
/// merely that one did.
fn serve_stderr(extra: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-runtime-host"));
    command.arg("serve");
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env(
        "AGENT_RUNTIME_LOCAL_STATE_ROOT",
        std::env::temp_dir().join("agent-runtime-surface-config-absent"),
    );
    command.env(
        "AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT",
        std::env::temp_dir().join("agent-runtime-surface-config-absent"),
    );
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().expect("runtime-host binary");
    assert!(
        !output.status.success(),
        "this configuration must not start a Runtime"
    );
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Naming a bind address without mTLS materials is a configuration error, not
/// an invitation to serve without them.
#[test]
fn a_bind_address_without_tls_refuses_to_start() {
    let stderr = serve_stderr(&[("AGENT_RUNTIME_INVOCATION_BIND", "127.0.0.1:0")]);

    assert!(
        stderr.contains("AGENT_RUNTIME_GRPC_SERVER_CERT"),
        "the refusal must name the missing material, got: {stderr}"
    );
}

/// mTLS alone is not enough: without a verifying key every bearer token would
/// be unverifiable, and a surface that cannot check tokens must not accept
/// connections.
#[test]
fn a_bind_address_without_a_verifying_key_refuses_to_start() {
    let certificates = std::env::temp_dir().join("agent-runtime-surface-config-certs");
    std::fs::create_dir_all(&certificates).expect("certificate directory");
    let certificate = certificates.join("server.pem");
    let key = certificates.join("server.key");
    let ca = certificates.join("ca.pem");
    for path in [&certificate, &key, &ca] {
        std::fs::write(path, b"not a real certificate\n").expect("write");
    }

    let stderr = serve_stderr(&[
        ("AGENT_RUNTIME_INVOCATION_BIND", "127.0.0.1:0"),
        (
            "AGENT_RUNTIME_GRPC_SERVER_CERT",
            certificate.to_str().unwrap(),
        ),
        ("AGENT_RUNTIME_GRPC_SERVER_KEY", key.to_str().unwrap()),
        ("AGENT_RUNTIME_GRPC_CLIENT_CA_CERT", ca.to_str().unwrap()),
    ]);

    std::fs::remove_dir_all(&certificates).ok();

    assert!(
        stderr.contains("AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY"),
        "the refusal must name the missing verifying key, got: {stderr}"
    );
}

/// The surface is off unless asked for.
///
/// An existing installation must not gain a network listener by upgrading, so
/// with no bind address the startup failure has to come from somewhere else
/// entirely -- proving the surface configuration was never consulted.
#[test]
fn no_bind_address_means_no_surface_and_no_tls_requirement() {
    let stderr = serve_stderr(&[]);

    for material in [
        "AGENT_RUNTIME_GRPC_SERVER_CERT",
        "AGENT_RUNTIME_GRPC_SERVER_KEY",
        "AGENT_RUNTIME_GRPC_CLIENT_CA_CERT",
        "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
    ] {
        assert!(
            !stderr.contains(material),
            "an unconfigured surface must not demand {material}, got: {stderr}"
        );
    }
}
