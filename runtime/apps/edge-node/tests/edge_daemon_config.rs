use agent_edge_node::daemon::EdgeDaemon;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer as _, SigningKey};
use std::collections::BTreeMap;

mod common;

/// The production break this catches is shipping an Edge binary that only
/// logs and exits, or requiring model credentials inline in a world-readable
/// config. A native daemon config must build the real enrolled Runtime and
/// outbound connector while reading the provider secret from an owner-only
/// file.
#[test]
fn native_daemon_config_builds_real_enrolled_runtime_without_inline_secret() {
    let root = tempfile::tempdir().expect("daemon root");
    let edge_state = root.path().join("edge-state");
    let runtime_state = root.path().join("runtime-state");
    let runtime_state_two = root.path().join("runtime-state-two");
    let workspace = root.path().join("workspace");
    let workspace_two = root.path().join("workspace-two");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&workspace_two).expect("workspace two");
    let now = chrono::Utc::now().timestamp_millis();
    let identity =
        agent_edge_node::EdgeDeviceIdentity::load_or_create(&edge_state).expect("identity");
    let manifest = common::capability_manifest();
    let control = SigningKey::from_bytes(&[101; 32]);
    let claims = agent_edge_node::EdgeEnrollmentGrantClaims {
        schema_version: 1,
        enrollment_id: uuid::Uuid::from_u128(4001),
        device_id: identity.device_id(),
        device_public_key_base64url: identity.public_key_base64url().into(),
        node_id: uuid::Uuid::from_u128(4002),
        node_generation: 1,
        capability_manifest_digest: manifest.digest().expect("manifest digest"),
        approved_capabilities: manifest.capabilities.clone(),
        issued_at_unix_ms: now - 1_000,
        expires_at_unix_ms: now - 1_000 + 24 * 60 * 60 * 1_000,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("grant claims"));
    let signed = format!("edge-enrollment-grant-v1.daemon-control-2026-08.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(control.sign(signed.as_bytes()).to_bytes());
    let grant_path = root.path().join("enrollment.grant");
    std::fs::write(&grant_path, format!("{signed}.{signature}")).expect("enrollment grant file");
    let secret_path = root.path().join("provider.key");
    std::fs::write(&secret_path, "provider-secret\n").expect("provider secret");
    let client_cert = root.path().join("client.pem");
    let client_key = root.path().join("client.key");
    let server_ca = root.path().join("ca.pem");
    for path in [&client_cert, &client_key, &server_ca] {
        std::fs::write(path, "-----BEGIN TEST-----\nvalue\n-----END TEST-----\n")
            .expect("TLS material");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [&grant_path, &secret_path, &client_key] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("owner-only secret");
        }
    }
    let config = serde_json::json!({
        "schema_version": 1,
        "edge_state_root": edge_state,
        "enrollment_grant_path": grant_path,
        "control_plane_public_keys": BTreeMap::from([(
            "daemon-control-2026-08",
            URL_SAFE_NO_PAD.encode(control.verifying_key().to_bytes()),
        )]),
        "control_plane_endpoint": "https://127.0.0.1:7443",
        "tls": {
            "client_certificate_path": client_cert,
            "client_private_key_path": client_key,
            "server_ca_path": server_ca,
            "server_domain_name": "edge-control.test"
        },
        "capability_manifest": manifest,
        "profiles": [
            {
                "runtime_state_root": runtime_state,
                "workspace_root": workspace,
                "invocation": {
                    "schema_version": 1,
                    "tenant_id": "00000000-0000-0000-0000-000000004101",
                    "application_id": "00000000-0000-0000-0000-000000004102",
                    "workload_identity_id": "00000000-0000-0000-0000-000000004103",
                    "workspace_id": "00000000-0000-0000-0000-000000004104",
                    "agent_version_id": "00000000-0000-0000-0000-000000004105",
                    "model_policy_id": "00000000-0000-0000-0000-000000004106"
                },
                "agent_instructions": "Run only the enrolled workspace Agent.",
                "provider": {
                    "id": "local-provider",
                    "protocol": "openai_compatible",
                    "endpoint": "http://127.0.0.1:8080/v1/chat/completions",
                    "model": "local-test-model",
                    "api_key_path": secret_path,
                    "region": "local",
                    "accepted_data_classes": ["internal"],
                    "capabilities": ["text", "tool_use"],
                    "latency_ms": 1,
                    "cost_per_million_tokens_micros": 1
                },
                "budget": {
                    "max_tokens": 1000,
                    "max_cost_cents": 100,
                    "max_duration_seconds": 60
                }
            },
            {
                "runtime_state_root": runtime_state_two,
                "workspace_root": workspace_two,
                "invocation": {
                    "schema_version": 1,
                    "tenant_id": "00000000-0000-0000-0000-000000004201",
                    "application_id": "00000000-0000-0000-0000-000000004202",
                    "workload_identity_id": "00000000-0000-0000-0000-000000004203",
                    "workspace_id": "00000000-0000-0000-0000-000000004204",
                    "agent_version_id": "00000000-0000-0000-0000-000000004205",
                    "model_policy_id": "00000000-0000-0000-0000-000000004206"
                },
                "agent_instructions": "Run the second tenant workspace Agent.",
                "provider": {
                    "id": "local-provider-two",
                    "protocol": "openai_responses",
                    "endpoint": "http://127.0.0.1:8081/v1/responses",
                    "model": "local-test-model-two",
                    "api_key_path": secret_path,
                    "region": "local",
                    "accepted_data_classes": ["internal"],
                    "capabilities": ["text", "tool_use"],
                    "latency_ms": 1,
                    "cost_per_million_tokens_micros": 1
                },
                "budget": {
                    "max_tokens": 1000,
                    "max_cost_cents": 100,
                    "max_duration_seconds": 60
                }
            }
        ]
    });
    let config_path = root.path().join("edge-node.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("config JSON"),
    )
    .expect("config");

    let daemon = EdgeDaemon::from_config_file(&config_path, now).expect("native daemon");

    assert_eq!(daemon.node_id(), uuid::Uuid::from_u128(4002));
    assert_eq!(daemon.node_generation(), 1);
    assert_eq!(daemon.profile_count(), 2);
    assert!(!format!("{daemon:?}").contains("provider-secret"));
}
