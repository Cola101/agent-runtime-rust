use agent_model_gateway::mcp::{McpFederationClient, McpServerRef};
use agent_model_gateway::mcp_oauth::{
    McpOAuthAuthorizationReason, McpOAuthAuthorizationRequest, McpOAuthBinding,
    McpOAuthCoordinator, McpOAuthCredentialStatus,
};
use agent_protocol::McpProtocolRevision;
use chrono::Utc;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use uuid::Uuid;

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("agent-mcp-oauth-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

struct TokenServer {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TokenServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn token_server() -> TokenServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed = Arc::clone(&observed);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4_096];
                let header_end = loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + content_length {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let body = String::from_utf8_lossy(&bytes[header_end..header_end + content_length]);
                let index = observed.fetch_add(1, Ordering::SeqCst);
                let payload = if body.contains("grant_type=authorization_code") {
                    assert!(body.contains("code=callback-code"));
                    assert!(body.contains("code_verifier="));
                    r#"{"access_token":"access-one","refresh_token":"refresh-one","token_type":"Bearer","expires_in":0,"scope":"tools.read"}"#
                } else {
                    assert!(body.contains("grant_type=refresh_token"));
                    assert!(body.contains("refresh_token=refresh-one"));
                    assert_eq!(index, 1, "only one refresh may contact the provider");
                    r#"{"access_token":"access-two","refresh_token":"refresh-two","token_type":"Bearer","expires_in":3600}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    TokenServer {
        endpoint,
        requests,
        task,
    }
}

async fn authenticated_mcp_server(
    expected_token: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4_096];
                let header_end = loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(
                    headers.to_ascii_lowercase().contains(
                        &format!("authorization: bearer {expected_token}").to_ascii_lowercase()
                    ),
                    "MCP request did not receive the credential-domain token"
                );
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap();
                while bytes.len() < header_end + content_length {
                    let read = socket.read(&mut chunk).await.unwrap();
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "server/discover" => serde_json::json!({
                        "resultType": "complete",
                        "supportedVersions": ["2026-07-28"],
                        "capabilities": {"tools": {}},
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "tools/list" => serde_json::json!({
                        "resultType": "complete",
                        "tools": [{
                            "name": "lookup",
                            "description": "lookup",
                            "inputSchema": {"type": "object"}
                        }],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    other => panic!("unexpected MCP method {other}"),
                };
                let response_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (endpoint, task)
}

fn binding(endpoint: &str) -> McpOAuthBinding {
    McpOAuthBinding {
        tenant_id: Uuid::now_v7(),
        server_id: Uuid::now_v7(),
        credential_id: Uuid::now_v7(),
        endpoint: endpoint.to_owned(),
    }
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn persisted_bytes(root: &Path) -> Vec<u8> {
    fn visit(path: &Path, output: &mut Vec<u8>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(&entry.path(), output);
            } else if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
                output.extend(std::fs::read(entry.path()).unwrap());
            }
        }
    }
    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

#[tokio::test]
async fn oauth_tokens_stay_encrypted_and_concurrent_refresh_is_singleflight() {
    let server = token_server().await;
    let root = TestRoot::new();
    let coordinator =
        McpOAuthCoordinator::new(root.path(), [7_u8; 32], Duration::from_secs(2), true).unwrap();
    let binding = binding(&server.endpoint);
    let now = chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
    let start = coordinator
        .begin_authorization(
            binding.clone(),
            McpOAuthAuthorizationRequest {
                authorization_endpoint: server.endpoint.replace("/token", "/authorize"),
                token_endpoint: server.endpoint.clone(),
                client_id: "public-client".into(),
                redirect_uri: "http://127.0.0.1/callback".into(),
                scopes: vec!["tools.read".into()],
            },
            now,
        )
        .await
        .unwrap();
    let authorization_url = reqwest::Url::parse(&start.authorization_url).unwrap();
    let returned_state = authorization_url
        .query_pairs()
        .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
        .unwrap();
    let first = coordinator
        .complete_authorization(
            binding.clone(),
            start.flow_id,
            &returned_state,
            "callback-code",
            now,
        )
        .await
        .unwrap();
    assert_eq!(first.token_digest(), digest("access-one"));

    let disk = persisted_bytes(root.path());
    for secret in [
        "access-one",
        "refresh-one",
        "callback-code",
        returned_state.as_str(),
    ] {
        assert!(
            !disk
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "credential material escaped the encrypted record"
        );
    }

    let one = {
        let coordinator = coordinator.clone();
        let binding = binding.clone();
        tokio::spawn(async move { coordinator.resolve_access_token(binding, now).await })
    };
    let two = {
        let coordinator = coordinator.clone();
        let binding = binding.clone();
        tokio::spawn(async move { coordinator.resolve_access_token(binding, now).await })
    };
    let one = one.await.unwrap().unwrap();
    let two = two.await.unwrap().unwrap();
    assert_eq!(one.token_digest(), digest("access-two"));
    assert_eq!(two.token_digest(), digest("access-two"));
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);

    assert_eq!(
        coordinator.status(binding.clone()).await.unwrap(),
        McpOAuthCredentialStatus::Active {
            expires_at: Some(now + chrono::Duration::seconds(3600)),
            revision: 5,
        }
    );
    assert!(
        !coordinator
            .record_rejected_access_token(binding.clone(), &digest("access-one"))
            .await
            .unwrap(),
        "a stale 401 must not overwrite a concurrently refreshed token"
    );
    assert!(
        coordinator
            .record_rejected_access_token(binding.clone(), &digest("access-two"))
            .await
            .unwrap()
    );
    assert_eq!(
        coordinator.status(binding).await.unwrap(),
        McpOAuthCredentialStatus::AuthorizationRequired {
            reason: McpOAuthAuthorizationReason::AccessTokenRejected,
            revision: 6,
        }
    );
}

#[tokio::test]
async fn a_credential_handle_is_bound_to_tenant_server_and_endpoint() {
    let server = token_server().await;
    let root = TestRoot::new();
    let coordinator =
        McpOAuthCoordinator::new(root.path(), [9_u8; 32], Duration::from_secs(2), true).unwrap();
    let original = binding(&server.endpoint);
    coordinator.revoke(original.clone()).await.unwrap();

    let mut another_tenant = original.clone();
    another_tenant.tenant_id = Uuid::now_v7();
    assert_eq!(
        coordinator.status(another_tenant).await.unwrap(),
        McpOAuthCredentialStatus::Absent
    );
    let mut another_server = original.clone();
    another_server.server_id = Uuid::now_v7();
    assert_eq!(
        coordinator.status(another_server).await.unwrap(),
        McpOAuthCredentialStatus::Absent
    );
    let mut redirected = original;
    redirected.endpoint = server.endpoint.replace("/token", "/other");
    assert!(coordinator.status(redirected).await.is_err());
}

#[tokio::test]
async fn federation_resolves_only_the_stable_handle_inside_the_gateway() {
    let token = token_server().await;
    let (mcp_endpoint, mcp_task) = authenticated_mcp_server("access-two").await;
    let root = TestRoot::new();
    let coordinator =
        McpOAuthCoordinator::new(root.path(), [11_u8; 32], Duration::from_secs(2), true).unwrap();
    let binding = binding(&mcp_endpoint);
    let now = chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
    let start = coordinator
        .begin_authorization(
            binding.clone(),
            McpOAuthAuthorizationRequest {
                authorization_endpoint: token.endpoint.replace("/token", "/authorize"),
                token_endpoint: token.endpoint.clone(),
                client_id: "public-client".into(),
                redirect_uri: "http://127.0.0.1/callback".into(),
                scopes: vec!["tools.read".into()],
            },
            now,
        )
        .await
        .unwrap();
    let returned_state = reqwest::Url::parse(&start.authorization_url)
        .unwrap()
        .query_pairs()
        .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
        .unwrap();
    coordinator
        .complete_authorization(
            binding.clone(),
            start.flow_id,
            &returned_state,
            "callback-code",
            now,
        )
        .await
        .unwrap();

    let client = McpFederationClient::for_open_servers(Duration::from_secs(2), true)
        .unwrap()
        .with_oauth_coordinator(Arc::new(coordinator));
    let server = McpServerRef {
        server_id: binding.server_id,
        name: "oauth".into(),
        endpoint: binding.endpoint,
        credential_envelope_json: String::new(),
        oauth_credential_id: Some(binding.credential_id),
        protocol_revision: McpProtocolRevision::V2026_07_28,
        client_capabilities: BTreeSet::new(),
    };
    let catalog = client.list_tools(binding.tenant_id, &server).await.unwrap();
    assert_eq!(catalog.tools[0].qualified_name, "mcp:oauth/lookup");
    assert_eq!(token.requests.load(Ordering::SeqCst), 2);
    mcp_task.abort();
}
