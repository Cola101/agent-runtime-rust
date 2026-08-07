use agent_nats_security::{NatsClientConfig, NatsSecurityError};
use std::path::PathBuf;

#[test]
fn secure_config_requires_credentials_and_a_ca_bundle() {
    let error = NatsClientConfig::new(
        "tls://nats.agent-runtime.svc:4222",
        "runtime-worker",
        " ",
        PathBuf::from("/var/run/secrets/nats/ca.pem"),
    )
    .unwrap_err();
    assert!(matches!(error, NatsSecurityError::BlankField("password")));

    let error = NatsClientConfig::new(
        "nats://nats.agent-runtime.svc:4222",
        "runtime-worker",
        "secret",
        PathBuf::from("/var/run/secrets/nats/ca.pem"),
    )
    .unwrap_err();
    assert!(matches!(error, NatsSecurityError::TlsRequired));
}

#[test]
fn debug_output_never_contains_the_password() {
    let config = NatsClientConfig::new(
        "tls://nats.agent-runtime.svc:4222",
        "runtime-worker",
        "very-sensitive-password",
        PathBuf::from("/var/run/secrets/nats/ca.pem"),
    )
    .unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("very-sensitive-password"));
    assert!(debug.contains("runtime-worker"));
}
