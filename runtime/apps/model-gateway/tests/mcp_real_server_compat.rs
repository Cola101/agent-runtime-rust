//! Compatibility with a real third-party MCP server.
//!
//! Everything else in this repo tests against an MCP server this repo wrote,
//! which proves the client agrees with itself. This one runs against the
//! official reference implementation (`@modelcontextprotocol/server-everything`,
//! Streamable HTTP transport) and found four ways the client was wrong.
//!
//! Ignored by default because it needs that server running. Bring it up and run:
//!
//! ```text
//! npm pack @modelcontextprotocol/server-everything && tar xzf modelcontext*.tgz
//! (cd package && npm install --omit=dev --ignore-scripts && node dist/index.js streamableHttp)
//! AGENT_RUNTIME_MCP_COMPAT_ENDPOINT=http://127.0.0.1:3001/mcp \
//!   cargo test -p agent-model-gateway --test mcp_real_server_compat -- --ignored
//! ```
//!
//! Ignored rather than skipped-when-unset on purpose: a skip counts as a pass in
//! the summary, and "the compatibility suite passed" would then be true on a
//! machine where it never ran.

use agent_model_gateway::mcp::{McpFederationClient, McpServerRef};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
use std::time::Duration;
use uuid::Uuid;

/// Every server this machine has been pointed at, comma separated.
///
/// Discovery only. These may be servers wired to real systems -- a mailbox, an
/// internal API -- and calling a tool on one would do whatever that tool does.
/// `tools/list` reads nothing outside the server itself, which is what protocol
/// compatibility actually needs.
fn discovery_endpoints() -> Vec<String> {
    std::env::var("AGENT_RUNTIME_MCP_COMPAT_ENDPOINTS")
        .expect("set AGENT_RUNTIME_MCP_COMPAT_ENDPOINTS to a comma-separated list of /mcp URLs")
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn endpoint() -> String {
    std::env::var("AGENT_RUNTIME_MCP_COMPAT_ENDPOINT")
        .expect("set AGENT_RUNTIME_MCP_COMPAT_ENDPOINT to the reference server's /mcp URL")
}

fn client() -> McpFederationClient {
    let pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    McpFederationClient::from_pkcs8_pem(&pem, Duration::from_secs(20), true).unwrap()
}

fn server() -> McpServerRef {
    McpServerRef {
        server_id: Uuid::now_v7(),
        name: "everything".into(),
        endpoint: endpoint(),
        credential_envelope_json: String::new(),
    }
}

/// Discovery against the reference server.
///
/// This is the test that failed four times before the client was conformant:
/// the Accept header had to name SSE as well as JSON (406 otherwise), the
/// response arrives as `text/event-stream` and has to be parsed as one, the
/// `Mcp-Session-Id` from initialize has to be echoed (400 otherwise), and the
/// initialized notification has no result to wait for.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the reference MCP server running; see the module comment"]
async fn discovery_works_against_the_reference_server() {
    let catalog = client()
        .list_tools(Uuid::now_v7(), &server())
        .await
        .expect("discovery against the reference server should succeed");

    assert!(
        !catalog.tools.is_empty(),
        "the reference server advertises tools"
    );
    assert_eq!(catalog.digest.len(), 64);
    assert!(
        catalog
            .tools
            .iter()
            .all(|tool| tool.qualified_name.starts_with("mcp:everything/")),
        "every tool must be namespaced under its server"
    );
    assert!(
        catalog
            .tools
            .iter()
            .any(|tool| tool.qualified_name == "mcp:everything/echo"),
        "the reference server's echo tool should be discovered, got {:?}",
        catalog
            .tools
            .iter()
            .map(|tool| &tool.qualified_name)
            .collect::<Vec<_>>()
    );
    // Its input schemas must survive the round trip as objects, since they are
    // what the model is shown.
    let echo = catalog
        .tools
        .iter()
        .find(|tool| tool.qualified_name == "mcp:everything/echo")
        .unwrap();
    let schema: serde_json::Value = serde_json::from_str(&echo.input_schema_json).unwrap();
    assert_eq!(schema["type"], "object");
}

/// A real call, and the freeze holding against a real server.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the reference MCP server running; see the module comment"]
async fn a_tool_call_round_trips_against_the_reference_server() {
    let client = client();
    let server = server();
    let tenant = Uuid::now_v7();
    let catalog = client.list_tools(tenant, &server).await.unwrap();

    let result = client
        .call_tool(
            tenant,
            &server,
            "mcp:everything/echo",
            r#"{"message":"agent runtime platform"}"#,
            &catalog.digest,
        )
        .await
        .expect("echo should round trip");

    assert!(!result.is_error);
    assert!(
        result.content_json.contains("agent runtime platform"),
        "the server's own answer should come back, got {}",
        result.content_json
    );

    // A digest this Run never froze must be refused even though the tool exists
    // and the server is perfectly healthy.
    let refused = client
        .call_tool(
            tenant,
            &server,
            "mcp:everything/echo",
            r#"{"message":"x"}"#,
            &"0".repeat(64),
        )
        .await
        .expect_err("a stale frozen digest must be refused");
    assert!(
        refused.to_string().contains("catalog changed"),
        "expected the catalog refusal, got {refused}"
    );
}

/// The same client against every third-party server available here.
///
/// One server proves the fixes work; several written by different people prove
/// they were fixes to the protocol rather than to one implementation's quirks.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs third-party MCP servers running; see the module comment"]
async fn discovery_works_against_every_configured_server() {
    let client = client();
    let endpoints = discovery_endpoints();
    assert!(
        !endpoints.is_empty(),
        "the list must name at least one server, or this asserts nothing"
    );

    for endpoint in endpoints {
        let server = McpServerRef {
            server_id: Uuid::now_v7(),
            name: "compat".into(),
            endpoint: endpoint.clone(),
            credential_envelope_json: String::new(),
        };
        let catalog = client
            .list_tools(Uuid::now_v7(), &server)
            .await
            .unwrap_or_else(|error| panic!("discovery against {endpoint} failed: {error}"));

        assert!(
            !catalog.tools.is_empty(),
            "{endpoint} advertised no tools, so nothing about the parse was exercised"
        );
        assert_eq!(catalog.digest.len(), 64, "{endpoint} digest");
        for tool in &catalog.tools {
            assert!(
                tool.qualified_name.starts_with("mcp:compat/"),
                "{endpoint} produced an unnamespaced tool: {}",
                tool.qualified_name
            );
            // The schema is what the model is shown, so it has to survive as an
            // object rather than as whatever the server happened to send.
            let schema: serde_json::Value = serde_json::from_str(&tool.input_schema_json)
                .unwrap_or_else(|error| {
                    panic!("{endpoint} tool {} has an unparseable schema: {error}",
                        tool.qualified_name)
                });
            assert!(
                schema.is_object(),
                "{endpoint} tool {} schema is not an object",
                tool.qualified_name
            );
        }
        println!(
            "compat: {endpoint} -> {} tools, digest {}",
            catalog.tools.len(),
            &catalog.digest[..16]
        );
    }
}

/// The sealed credential path, against a server that actually checks it.
///
/// Every other test here runs against an open server, so the credential code
/// could have been silently broken and nothing would have noticed. This one
/// seals a real bearer token into a real envelope, has the client open it, and
/// talks to a server that answers 401 without it.
///
/// The unauthenticated half is the load-bearing part: without it this test would
/// pass even if the envelope were ignored entirely, which is exactly the shape
/// of a test that checks nothing.
///
/// ```text
/// AGENT_RUNTIME_MCP_COMPAT_AUTH_ENDPOINT=https://api.githubcopilot.com/mcp/ \
/// AGENT_RUNTIME_MCP_COMPAT_BEARER="$(gh auth token)" \
///   cargo test -p agent-model-gateway --test mcp_real_server_compat -- --ignored sealed
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an authenticating public MCP server and a token; see the doc comment"]
async fn a_sealed_credential_opens_against_an_authenticating_server() {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use base64::Engine;
    use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
    use rsa::rand_core::RngCore;
    use rsa::{Oaep, RsaPublicKey};
    use sha2::{Digest, Sha256};

    let endpoint = std::env::var("AGENT_RUNTIME_MCP_COMPAT_AUTH_ENDPOINT")
        .expect("set AGENT_RUNTIME_MCP_COMPAT_AUTH_ENDPOINT");
    let bearer = std::env::var("AGENT_RUNTIME_MCP_COMPAT_BEARER")
        .expect("set AGENT_RUNTIME_MCP_COMPAT_BEARER");

    let pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    let private_key = RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let key_id = hex::encode(Sha256::digest(
        public_key.to_public_key_der().unwrap().as_ref(),
    ));
    let client = McpFederationClient::from_pkcs8_pem(&pem, Duration::from_secs(30), false).unwrap();

    let tenant = Uuid::now_v7();
    let server_id = Uuid::now_v7();
    let mut data_key = [0_u8; 32];
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut data_key);
    OsRng.fill_bytes(&mut nonce);
    let encrypted_key = public_key
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &data_key)
        .unwrap();
    let aad = format!("{tenant}:{server_id}");
    let ciphertext = Aes256Gcm::new_from_slice(&data_key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: bearer.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    let base64 = base64::engine::general_purpose::STANDARD;
    let envelope = serde_json::json!({
        "schema_version": 1,
        "key_id": key_id,
        "algorithm": "RSA-OAEP-256+A256GCM",
        "encrypted_key": base64.encode(&encrypted_key),
        "nonce": base64.encode(nonce),
        "ciphertext": base64.encode(&ciphertext),
    })
    .to_string();

    // Without the envelope the server must refuse, or the sealed run below
    // proves nothing about the sealing.
    let open = McpServerRef {
        server_id,
        name: "github".into(),
        endpoint: endpoint.clone(),
        credential_envelope_json: String::new(),
    };
    let refused = client
        .list_tools(tenant, &open)
        .await
        .expect_err("an authenticating server must refuse an unauthenticated client");
    assert!(
        refused.to_string().contains("401"),
        "expected an auth refusal, got {refused}"
    );

    let sealed = McpServerRef {
        credential_envelope_json: envelope,
        ..open
    };
    let catalog = client
        .list_tools(tenant, &sealed)
        .await
        .expect("a sealed credential should open and authenticate");

    assert!(!catalog.tools.is_empty());
    assert!(
        catalog
            .tools
            .iter()
            .all(|tool| tool.qualified_name.starts_with("mcp:github/")),
        "every tool must be namespaced under its server"
    );
    println!(
        "compat: {endpoint} authenticated -> {} tools, digest {}",
        catalog.tools.len(),
        &catalog.digest[..16]
    );
}
