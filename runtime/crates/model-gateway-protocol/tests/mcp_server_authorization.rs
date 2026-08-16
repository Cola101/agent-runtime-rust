use agent_model_gateway_protocol::{mcp_server_authorization_digest, v1::McpServerRef};

/// The production break this catches is authorizing only an MCP `server_id`.
/// Every field that can redirect authority or change the delegated protocol
/// must contribute to one stable, cross-language digest.
#[test]
fn digest_binds_the_exact_wire_server_snapshot() {
    let server = McpServerRef {
        server_id: "0198f4f8-0b6d-7a31-9fa2-3ad2e1cb42d1".into(),
        name: "billing".into(),
        endpoint: "https://mcp.example.test/v1".into(),
        credential_envelope_json: br#"{"ciphertext":"abc"}"#.to_vec(),
        protocol_revision: "2025-06-18".into(),
        client_capabilities: vec!["sampling".into(), "elicitation".into()],
        oauth_credential_id: String::new(),
    };

    assert_eq!(
        mcp_server_authorization_digest(&server),
        "8dcdc890a0f13d781776124c750b806e246f301d8006834c4baca7aa46c924bf"
    );

    let mut redirected = server.clone();
    redirected.endpoint = "https://attacker.example.test/v1".into();
    assert_ne!(
        mcp_server_authorization_digest(&server),
        mcp_server_authorization_digest(&redirected)
    );
}

#[test]
fn oauth_handle_changes_the_authorized_server_identity_without_exposing_a_token() {
    let mut server = McpServerRef {
        server_id: "0198f4f8-0b6d-7a31-9fa2-3ad2e1cb42d1".into(),
        name: "billing".into(),
        endpoint: "https://mcp.example.test/v1".into(),
        credential_envelope_json: Vec::new(),
        protocol_revision: "2025-06-18".into(),
        client_capabilities: Vec::new(),
        oauth_credential_id: "0198f4f8-0b6d-7a31-9fa2-3ad2e1cb42d2".into(),
    };
    let first = mcp_server_authorization_digest(&server);

    server.oauth_credential_id = "0198f4f8-0b6d-7a31-9fa2-3ad2e1cb42d3".into();
    assert_ne!(first, mcp_server_authorization_digest(&server));
    assert!(!first.contains(&server.oauth_credential_id));
}
