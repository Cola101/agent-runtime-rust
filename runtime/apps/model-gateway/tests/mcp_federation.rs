//! Federated MCP calls against a real HTTP server on loopback.
//!
//! The unit tests in the module cover the pure parts. These exist because the
//! parts that break in practice are the ones that involve a socket: whether the
//! credential actually reaches the server, whether a changed catalog is really
//! refused, and whether an oversized body is stopped rather than read.

use agent_model_gateway::mcp::{McpFederationClient, McpFederationError, McpServerRef};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const TEST_KEY_ID: &str = "mcp-test-key";

/// Generated once for the whole suite rather than committed as a file. A private
/// key in the repository is a private key in the repository even when it is only
/// for tests, and RSA keygen is slow enough that per-test generation would
/// dominate the run.
fn test_key_pem() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        RsaPrivateKey::new(&mut OsRng, 2048)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    })
}

struct ServerBehaviour {
    /// Tool names the server advertises. Swapping this mid-test is how a
    /// catalog change is simulated.
    tools: Vec<String>,
    /// Every Authorization header the server saw, so a test can assert the
    /// credential arrived rather than assuming it did.
    seen_authorization: Vec<Option<String>>,
    /// Bytes of filler appended to a tools/call result.
    padding: usize,
}

async fn spawn_server(behaviour: Arc<Mutex<ServerBehaviour>>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let behaviour = Arc::clone(&behaviour);
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 64 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                counter.fetch_add(1, Ordering::SeqCst);
                let authorization = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                    .map(|line| line["authorization:".len()..].trim().to_owned());
                let body = {
                    let mut state = behaviour.lock().unwrap();
                    state.seen_authorization.push(authorization);
                    if request.contains("\"tools/list\"") {
                        let tools = state
                            .tools
                            .iter()
                            .map(|name| {
                                format!(
                                    r#"{{"name":"{name}","description":"d",
                                        "inputSchema":{{"type":"object"}}}}"#
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools}]}}}}"#)
                    } else if request.contains("\"tools/call\"") {
                        let filler = "x".repeat(state.padding);
                        format!(
                            r#"{{"jsonrpc":"2.0","id":1,"result":{{"content":[{{"type":"text","text":"ok{filler}"}}],"isError":false}}}}"#
                        )
                    } else {
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.to_owned()
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    (format!("http://{address}/rpc"), requests)
}

fn client() -> McpFederationClient {
    McpFederationClient::from_pkcs8_pem(test_key_pem(), TEST_KEY_ID, Duration::from_secs(5))
        .unwrap()
}

fn open_server(endpoint: String) -> McpServerRef {
    McpServerRef {
        server_id: Uuid::now_v7(),
        name: "search".into(),
        endpoint,
        credential_envelope_json: String::new(),
    }
}

fn behaviour(tools: &[&str]) -> Arc<Mutex<ServerBehaviour>> {
    Arc::new(Mutex::new(ServerBehaviour {
        tools: tools.iter().map(|name| (*name).to_owned()).collect(),
        seen_authorization: Vec::new(),
        padding: 0,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_qualifies_every_tool_under_its_server() {
    let state = behaviour(&["web_search", "fetch"]);
    let (endpoint, _) = spawn_server(Arc::clone(&state)).await;

    let catalog = client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .expect("discovery should succeed");

    assert_eq!(
        catalog
            .tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:search/fetch", "mcp:search/web_search"],
        "tools must be namespaced by server and ordered, so the digest is stable"
    );
    assert!(!catalog.digest.is_empty());
}

/// The whole point of freezing a catalog: a server that changes what it offers
/// mid-Run must not have the change take effect inside a Run approved against
/// the old catalog.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_is_refused_once_the_server_changes_its_catalog() {
    let state = behaviour(&["web_search"]);
    let (endpoint, _) = spawn_server(Arc::clone(&state)).await;
    let server = open_server(endpoint);
    let tenant = Uuid::now_v7();
    let frozen = client()
        .list_tools(tenant, &server)
        .await
        .unwrap()
        .digest;

    // Same call succeeds while the catalog matches.
    client()
        .call_tool(tenant, &server, "mcp:search/web_search", "{}", &frozen)
        .await
        .expect("a call against the frozen catalog should succeed");

    state.lock().unwrap().tools = vec!["web_search".into(), "delete_everything".into()];

    let refused = client()
        .call_tool(tenant, &server, "mcp:search/web_search", "{}", &frozen)
        .await
        .expect_err("the catalog changed, so the call must be refused");
    assert!(
        matches!(refused, McpFederationError::CatalogChanged),
        "expected CatalogChanged, got {refused:?}"
    );
}

/// A tool the Run never froze is refused even when the server offers it and the
/// digest still matches -- the second check is not implied by the first.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_outside_the_frozen_catalog_is_refused() {
    let state = behaviour(&["web_search"]);
    let (endpoint, _) = spawn_server(state).await;
    let server = open_server(endpoint);
    let tenant = Uuid::now_v7();
    let frozen = client().list_tools(tenant, &server).await.unwrap().digest;

    let refused = client()
        .call_tool(tenant, &server, "mcp:search/not_offered", "{}", &frozen)
        .await
        .expect_err("a tool absent from the catalog must be refused");
    assert!(
        matches!(refused, McpFederationError::ToolNotInFrozenCatalog(_)),
        "expected ToolNotInFrozenCatalog, got {refused:?}"
    );
}

/// An unauthenticated server must not receive an Authorization header at all.
/// Sending an empty or placeholder one would be a credential that is not a
/// credential, and would look authenticated in the server's logs.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_server_receives_no_authorization_header() {
    let state = behaviour(&["web_search"]);
    let (endpoint, _) = spawn_server(Arc::clone(&state)).await;

    client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .unwrap();

    let seen = state.lock().unwrap().seen_authorization.clone();
    assert!(!seen.is_empty(), "the server should have seen requests");
    assert!(
        seen.iter().all(Option::is_none),
        "an open server must not be sent an Authorization header, saw {seen:?}"
    );
}

/// Untrusted third-party content headed for the model's context has to be
/// bounded, or one server can exhaust a Run by answering at length.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_response_is_refused_rather_than_read() {
    let state = behaviour(&["web_search"]);
    state.lock().unwrap().padding = 512 * 1024;
    let (endpoint, _) = spawn_server(Arc::clone(&state)).await;
    let server = open_server(endpoint);
    let tenant = Uuid::now_v7();
    let frozen = client().list_tools(tenant, &server).await.unwrap().digest;

    let refused = client()
        .call_tool(tenant, &server, "mcp:search/web_search", "{}", &frozen)
        .await
        .expect_err("an oversized body must be refused");
    assert!(
        matches!(refused, McpFederationError::ResponseTooLarge),
        "expected ResponseTooLarge, got {refused:?}"
    );
}

/// A server naming a tool with the qualified-name separator could make a call to
/// one tool resolve as another. The server chose the string, so it is checked
/// rather than trusted.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_name_carrying_the_separator_is_refused() {
    for hostile in ["other/tool", "mcp:other", ""] {
        let state = behaviour(&[hostile]);
        let (endpoint, _) = spawn_server(state).await;

        let refused = client()
            .list_tools(Uuid::now_v7(), &open_server(endpoint))
            .await
            .expect_err("a tool name that cannot be qualified must be refused");
        assert!(
            matches!(refused, McpFederationError::Protocol(_)),
            "name {hostile:?}: expected Protocol, got {refused:?}"
        );
    }
}

/// The positive half of the credential story: a sealed envelope is opened here
/// and the secret inside reaches the server as a bearer token.
///
/// Asserting only that an open server gets no header would leave "the seal is
/// never opened successfully" passing every test.
#[tokio::test(flavor = "multi_thread")]
async fn a_sealed_credential_is_opened_and_sent_as_a_bearer_token() {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use base64::Engine;
    use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
    use rsa::rand_core::RngCore;
    use rsa::{Oaep, RsaPublicKey};
    use sha2::{Digest, Sha256};

    let private_key = RsaPrivateKey::from_pkcs8_pem(test_key_pem()).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let key_id = hex::encode(Sha256::digest(
        public_key.to_public_key_der().unwrap().as_ref(),
    ));

    let tenant = Uuid::now_v7();
    let server_id = Uuid::now_v7();
    let mut data_key = [0_u8; 32];
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut data_key);
    OsRng.fill_bytes(&mut nonce);
    let encrypted_key = public_key
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &data_key)
        .unwrap();
    // The AAD binds the envelope to this tenant and this server, so one lifted
    // from another row cannot be replayed here.
    let aad = format!("{tenant}:{server_id}");
    let ciphertext = Aes256Gcm::new_from_slice(&data_key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: b"tenant-mcp-secret",
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

    let state = behaviour(&["web_search"]);
    let (endpoint, _) = spawn_server(Arc::clone(&state)).await;
    let client =
        McpFederationClient::from_pkcs8_pem(test_key_pem(), key_id, Duration::from_secs(5))
            .unwrap();

    client
        .list_tools(
            tenant,
            &McpServerRef {
                server_id,
                name: "search".into(),
                endpoint,
                credential_envelope_json: envelope,
            },
        )
        .await
        .expect("a sealed credential should open");

    let seen = state.lock().unwrap().seen_authorization.clone();
    assert!(
        seen.iter()
            .all(|header| header.as_deref() == Some("Bearer tenant-mcp-secret")),
        "every request should carry the opened credential, saw {seen:?}"
    );
}

/// An envelope sealed for a different server must not open here, or a tenant
/// holding one server's row could authenticate as another.
#[tokio::test(flavor = "multi_thread")]
async fn an_envelope_sealed_for_another_server_does_not_open() {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use base64::Engine;
    use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
    use rsa::rand_core::RngCore;
    use rsa::{Oaep, RsaPublicKey};
    use sha2::{Digest, Sha256};

    let private_key = RsaPrivateKey::from_pkcs8_pem(test_key_pem()).unwrap();
    let public_key = RsaPublicKey::from(&private_key);
    let key_id = hex::encode(Sha256::digest(
        public_key.to_public_key_der().unwrap().as_ref(),
    ));
    let tenant = Uuid::now_v7();
    let sealed_for = Uuid::now_v7();
    let presented_as = Uuid::now_v7();

    let mut data_key = [0_u8; 32];
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut data_key);
    OsRng.fill_bytes(&mut nonce);
    let encrypted_key = public_key
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &data_key)
        .unwrap();
    let aad = format!("{tenant}:{sealed_for}");
    let ciphertext = Aes256Gcm::new_from_slice(&data_key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: b"tenant-mcp-secret",
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

    let (endpoint, _) = spawn_server(behaviour(&["web_search"])).await;
    let client =
        McpFederationClient::from_pkcs8_pem(test_key_pem(), key_id, Duration::from_secs(5))
            .unwrap();

    let refused = client
        .list_tools(
            tenant,
            &McpServerRef {
                server_id: presented_as,
                name: "search".into(),
                endpoint,
                credential_envelope_json: envelope,
            },
        )
        .await
        .expect_err("an envelope sealed for another server must not open");
    assert!(
        matches!(refused, McpFederationError::CredentialUnopenable),
        "expected CredentialUnopenable, got {refused:?}"
    );
}
