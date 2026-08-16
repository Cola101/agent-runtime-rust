//! Federated MCP calls against a real HTTP server on loopback.
//!
//! The unit tests in the module cover the pure parts. These exist because the
//! parts that break in practice are the ones that involve a socket: whether the
//! credential actually reaches the server, whether a changed catalog is really
//! refused, and whether an oversized body is stopped rather than read.

use agent_model_gateway::mcp::{
    McpCallLifecycle, McpFederationClient, McpFederationError, McpRoundTripContinuation,
    McpServerRef, McpToolCallOutcome,
};
use agent_protocol::{
    McpClientCapability, McpInputAction, McpInputResponse, McpProtocolRevision, McpServerCapability,
};
use rsa::RsaPrivateKey;
use rsa::rand_core::OsRng;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

/// Generated once for the whole suite rather than committed as a file. A private
/// key in the repository is a private key in the repository even when it is only
/// for tests, and RSA keygen is slow enough that per-test generation would
/// dominate the run.
fn test_key_pem() -> &'static str {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        RsaPrivateKey::new(&mut OsRng, 3072)
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
    /// Handshake protocol selected by the server.
    protocol_version: String,
    /// Whether the server negotiated the tools capability before tool traffic.
    advertise_tools: bool,
    advertise_resources: bool,
    advertise_prompts: bool,
    /// Optional number of actual initialize handshakes that may still advertise
    /// Tools. Notifications do not consume this counter.
    advertise_tools_for_initializes: Option<usize>,
    /// JSON-RPC response id echoed by the fixture.
    response_id: u64,
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
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[{tools}]}}}}"#,
                            state.response_id
                        )
                    } else if request.contains("\"tools/call\"") {
                        let filler = "x".repeat(state.padding);
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{},"result":{{"content":[{{"type":"text","text":"ok{filler}"}}],"isError":false}}}}"#,
                            state.response_id
                        )
                    } else {
                        let mut advertised = Vec::new();
                        let advertise_tools = if request.contains("\"method\":\"initialize\"") {
                            match state.advertise_tools_for_initializes.as_mut() {
                                Some(remaining) => {
                                    let advertised = *remaining > 0;
                                    *remaining = remaining.saturating_sub(1);
                                    advertised
                                }
                                None => state.advertise_tools,
                            }
                        } else {
                            state.advertise_tools
                        };
                        if advertise_tools {
                            advertised.push(r#""tools":{}"#);
                        }
                        if state.advertise_resources {
                            advertised.push(r#""resources":{"listChanged":true}"#);
                        }
                        if state.advertise_prompts {
                            advertised.push(r#""prompts":{"listChanged":false}"#);
                        }
                        let capabilities = format!("{{{}}}", advertised.join(","));
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"{}","capabilities":{capabilities}}}}}"#,
                            state.response_id, state.protocol_version
                        )
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

async fn spawn_read_surface_server() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
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
                while bytes.len() - header_end < content_length {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap();
                recorded.lock().unwrap().push(request.clone());
                let id = request.get("id").cloned().unwrap_or(serde_json::json!(0));
                let result = match request["method"].as_str().unwrap_or_default() {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"resources": {}, "prompts": {}},
                        "serverInfo": {"name": "read-surface", "version": "1"}
                    }),
                    "resources/list" => {
                        assert_eq!(
                            request.pointer("/params/cursor"),
                            Some(&serde_json::json!("r/1"))
                        );
                        serde_json::json!({
                            "resources": [{
                                "uri": "kb://tenant/runbook",
                                "name": "runbook",
                                "mimeType": "text/markdown",
                                "size": 12
                            }],
                            "nextCursor": "r/2"
                        })
                    }
                    "resources/read" => {
                        assert_eq!(
                            request.pointer("/params/uri"),
                            Some(&serde_json::json!("kb://tenant/runbook"))
                        );
                        serde_json::json!({
                            "contents": [
                                {"uri": "kb://tenant/runbook", "text": "hello"},
                                {"uri": "blob://tenant/a", "blob": "AAEC"}
                            ]
                        })
                    }
                    "resources/templates/list" => {
                        assert_eq!(
                            request.pointer("/params/cursor"),
                            Some(&serde_json::json!("t/1"))
                        );
                        serde_json::json!({
                            "resourceTemplates": [{
                                "uriTemplate": "kb://tenant/{name}",
                                "name": "knowledge",
                                "mimeType": "text/markdown"
                            }],
                            "nextCursor": "t/2"
                        })
                    }
                    "prompts/list" => {
                        assert_eq!(
                            request.pointer("/params/cursor"),
                            Some(&serde_json::json!("p/1"))
                        );
                        serde_json::json!({
                            "prompts": [{
                                "name": "summarize",
                                "description": "Summarize",
                                "arguments": [{"name": "tone", "required": false}]
                            }],
                            "nextCursor": "p/2"
                        })
                    }
                    "prompts/get" => {
                        assert_eq!(
                            request.pointer("/params/arguments/tone"),
                            Some(&serde_json::json!("short"))
                        );
                        serde_json::json!({
                            "description": "resolved",
                            "messages": [{
                                "role": "user",
                                "content": {"type": "text", "text": "Summarize this"}
                            }]
                        })
                    }
                    "notifications/initialized" => serde_json::json!({}),
                    method => panic!("unexpected MCP method {method}"),
                };
                let body =
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (format!("http://{address}/rpc"), seen)
}

async fn spawn_modern_read_surface_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.contains("mcp-protocol-version: 2026-07-28"));
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap();
                while bytes.len() - header_end < content_length {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "server/discover" => serde_json::json!({
                        "resultType": "complete",
                        "supportedVersions": ["2026-07-28"],
                        "capabilities": {"resources": {}, "prompts": {}},
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "resources/list" => serde_json::json!({
                        "resultType": "complete",
                        "resources": [{"uri": "kb://modern/runbook", "name": "runbook"}],
                        "nextCursor": "modern-r2",
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "resources/read" => serde_json::json!({
                        "resultType": "complete",
                        "contents": [{"uri": "kb://modern/runbook", "text": "modern"}],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "resources/templates/list" => serde_json::json!({
                        "resultType": "complete",
                        "resourceTemplates": [{
                            "uriTemplate": "kb://modern/{name}",
                            "name": "knowledge"
                        }],
                        "nextCursor": "modern-t2",
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "prompts/list" => serde_json::json!({
                        "resultType": "complete",
                        "prompts": [{"name": "summarize"}],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "prompts/get" => serde_json::json!({
                        "resultType": "complete",
                        "messages": [{
                            "role": "assistant",
                            "content": {"type": "text", "text": "modern prompt"}
                        }],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    method => panic!("unexpected modern read method {method}"),
                };
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": result
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    format!("http://{address}/mcp")
}

async fn spawn_revoking_resource_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let initializes = Arc::new(AtomicUsize::new(0));
    let resource_calls = Arc::new(AtomicUsize::new(0));
    let observed_resource_calls = Arc::clone(&resource_calls);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let initializes = Arc::clone(&initializes);
            let resource_calls = Arc::clone(&resource_calls);
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]);
                let (status, body) = if request.contains("notifications/initialized") {
                    ("202 Accepted", String::new())
                } else if request.contains("resources/list") {
                    resource_calls.fetch_add(1, Ordering::SeqCst);
                    (
                        "200 OK",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"resources":[]}}"#.to_owned(),
                    )
                } else {
                    let ordinal = initializes.fetch_add(1, Ordering::SeqCst) + 1;
                    let capabilities = if ordinal <= 2 {
                        r#"{"resources":{}}"#
                    } else {
                        r#"{"prompts":{}}"#
                    };
                    (
                        "200 OK",
                        format!(
                            r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{capabilities}}}}}"#
                        ),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (format!("http://{address}/rpc"), observed_resource_calls)
}

fn client() -> McpFederationClient {
    McpFederationClient::from_pkcs8_pem(test_key_pem(), Duration::from_secs(5), true).unwrap()
}

fn open_server(endpoint: String) -> McpServerRef {
    McpServerRef {
        server_id: Uuid::now_v7(),
        name: "search".into(),
        endpoint,
        credential_envelope_json: String::new(),
        oauth_credential_id: None,
        protocol_revision: McpProtocolRevision::V2025_06_18,
        client_capabilities: BTreeSet::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_resources_and_prompts_cross_a_real_http_mcp_session() {
    let (endpoint, seen) = spawn_read_surface_server().await;
    let server = open_server(endpoint);
    let tenant_id = Uuid::now_v7();
    let client = client();
    let catalog = client.list_tools(tenant_id, &server).await.unwrap();
    assert!(catalog.tools.is_empty());
    assert_eq!(
        catalog.capabilities,
        BTreeSet::from([McpServerCapability::Resources, McpServerCapability::Prompts])
    );

    let resources = client
        .list_resources(tenant_id, &server, &catalog.digest, Some("r/1"))
        .await
        .unwrap();
    assert_eq!(resources.resources[0].uri, "kb://tenant/runbook");
    assert_eq!(resources.next_cursor.as_deref(), Some("r/2"));

    let read = client
        .read_resource(tenant_id, &server, &catalog.digest, "kb://tenant/runbook")
        .await
        .unwrap();
    assert_eq!(read.contents.len(), 2);

    let templates = client
        .list_resource_templates(tenant_id, &server, &catalog.digest, Some("t/1"))
        .await
        .unwrap();
    assert_eq!(templates.resource_templates[0].name, "knowledge");
    assert_eq!(templates.next_cursor.as_deref(), Some("t/2"));

    let prompts = client
        .list_prompts(tenant_id, &server, &catalog.digest, Some("p/1"))
        .await
        .unwrap();
    assert_eq!(prompts.prompts[0].name, "summarize");
    assert_eq!(prompts.next_cursor.as_deref(), Some("p/2"));

    let prompt = client
        .get_prompt(
            tenant_id,
            &server,
            &catalog.digest,
            "summarize",
            Some(&serde_json::json!({"tone": "short"})),
        )
        .await
        .unwrap();
    assert_eq!(prompt.description.as_deref(), Some("resolved"));
    assert_eq!(prompt.messages[0].role, "user");

    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["method"] == "tools/list"),
        "a resource/prompt-only server must never receive tools/list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn modern_resources_and_prompts_use_the_same_protocol_neutral_contract() {
    let mut server = open_server(spawn_modern_read_surface_server().await);
    server.protocol_revision = McpProtocolRevision::V2026_07_28;
    let tenant_id = Uuid::now_v7();
    let client = client();
    let catalog = client.list_tools(tenant_id, &server).await.unwrap();
    assert_eq!(
        catalog.capabilities,
        BTreeSet::from([McpServerCapability::Resources, McpServerCapability::Prompts])
    );

    let resources = client
        .list_resources(tenant_id, &server, &catalog.digest, None)
        .await
        .unwrap();
    assert_eq!(resources.resources[0].uri, "kb://modern/runbook");
    assert_eq!(resources.next_cursor.as_deref(), Some("modern-r2"));
    assert_eq!(
        client
            .read_resource(tenant_id, &server, &catalog.digest, "kb://modern/runbook")
            .await
            .unwrap()
            .contents
            .len(),
        1
    );
    assert_eq!(
        client
            .list_resource_templates(tenant_id, &server, &catalog.digest, None)
            .await
            .unwrap()
            .resource_templates[0]
            .uri_template,
        "kb://modern/{name}"
    );
    assert_eq!(
        client
            .list_prompts(tenant_id, &server, &catalog.digest, None)
            .await
            .unwrap()
            .prompts[0]
            .name,
        "summarize"
    );
    assert_eq!(
        client
            .get_prompt(tenant_id, &server, &catalog.digest, "summarize", None)
            .await
            .unwrap()
            .messages[0]
            .role,
        "assistant"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_capability_revoked_on_the_operation_session_fails_before_read() {
    let (endpoint, resource_calls) = spawn_revoking_resource_server().await;
    let server = open_server(endpoint);
    let tenant_id = Uuid::now_v7();
    let client = client();
    let catalog = client.list_tools(tenant_id, &server).await.unwrap();
    assert!(
        catalog
            .capabilities
            .contains(&McpServerCapability::Resources)
    );

    let error = client
        .list_resources(tenant_id, &server, &catalog.digest, None)
        .await
        .expect_err("the operation session revoked Resources");

    assert!(matches!(error, McpFederationError::CatalogChanged));
    assert_eq!(
        resource_calls.load(Ordering::SeqCst),
        0,
        "resources/list was sent after the fresh session revoked capability"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn modern_http_mrtr_preserves_opaque_state_and_uses_a_fresh_request_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let header_end = loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.contains("mcp-protocol-version: 2026-07-28"));
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap();
                while bytes.len() - header_end < content_length {
                    let read = socket.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap();
                recorded.lock().unwrap().push(body.clone());
                let result = match body["method"].as_str().unwrap() {
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
                            "name": "confirm",
                            "description": "confirm",
                            "inputSchema": {"type": "object"}
                        }],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "tools/call" if body.pointer("/params/inputResponses").is_none() => {
                        serde_json::json!({
                            "resultType": "input_required",
                            "inputRequests": {
                                "confirmation": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "form",
                                        "message": "Confirm",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": {"confirmed": {"type": "boolean"}},
                                            "required": ["confirmed"]
                                        }
                                    }
                                }
                            },
                            "requestState": " opaque/\u{2603}/=?base64?literal?=\n"
                        })
                    }
                    "tools/call" => {
                        assert_eq!(
                            body.pointer("/params/requestState"),
                            Some(&serde_json::json!(" opaque/\u{2603}/=?base64?literal?=\n"))
                        );
                        assert_eq!(
                            body.pointer("/params/inputResponses/confirmation/action"),
                            Some(&serde_json::json!("accept"))
                        );
                        serde_json::json!({
                            "resultType": "complete",
                            "content": [{"type": "text", "text": "confirmed"}],
                            "isError": false
                        })
                    }
                    method => panic!("unexpected method {method}"),
                };
                let response_body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": result
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    let server = McpServerRef {
        server_id: Uuid::now_v7(),
        name: "modern".into(),
        endpoint,
        credential_envelope_json: String::new(),
        oauth_credential_id: None,
        protocol_revision: McpProtocolRevision::V2026_07_28,
        client_capabilities: BTreeSet::from([McpClientCapability::Elicitation]),
    };
    let client = client();
    let tenant = Uuid::now_v7();
    let catalog = client.list_tools(tenant, &server).await.unwrap();
    let first = client
        .call_tool_round(
            tenant,
            &server,
            "mcp:modern/confirm",
            "{}",
            &catalog.digest,
            None,
        )
        .await
        .unwrap();
    let McpToolCallOutcome::InputRequired(required) = first else {
        panic!("first round must request input")
    };
    assert_eq!(required.round, 1);
    assert_eq!(
        required.request_state,
        " opaque/\u{2603}/=?base64?literal?=\n"
    );

    let continuation = McpRoundTripContinuation {
        round: 2,
        request_state: required.request_state,
        responses: BTreeMap::from([(
            "confirmation".into(),
            McpInputResponse {
                action: McpInputAction::Accept,
                content: Some(serde_json::json!({"confirmed": true})),
                meta: None,
            },
        )]),
    };
    let completed = client
        .call_tool_round(
            tenant,
            &server,
            "mcp:modern/confirm",
            "{}",
            &catalog.digest,
            Some(&continuation),
        )
        .await
        .unwrap();
    assert!(matches!(completed, McpToolCallOutcome::Complete(_)));

    let calls = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body["method"] == "tools/call")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["id"], calls[1]["id"]);
}

fn behaviour(tools: &[&str]) -> Arc<Mutex<ServerBehaviour>> {
    Arc::new(Mutex::new(ServerBehaviour {
        tools: tools.iter().map(|name| (*name).to_owned()).collect(),
        seen_authorization: Vec::new(),
        padding: 0,
        protocol_version: "2025-06-18".into(),
        advertise_tools: true,
        advertise_resources: false,
        advertise_prompts: false,
        advertise_tools_for_initializes: None,
        response_id: 1,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_refuses_a_server_that_selects_an_unsupported_protocol() {
    let state = behaviour(&["web_search"]);
    state.lock().unwrap().protocol_version = "2025-03-26".into();
    let (endpoint, requests) = spawn_server(state).await;

    let refused = client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .expect_err("an unsupported negotiated protocol must fail closed");

    assert!(matches!(refused, McpFederationError::Protocol(_)));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "no initialized notification or tools/list may follow a rejected handshake"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_refuses_tool_traffic_without_a_negotiated_tools_capability() {
    let state = behaviour(&["web_search"]);
    state.lock().unwrap().advertise_tools = false;
    let (endpoint, requests) = spawn_server(state).await;

    let refused = client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .expect_err("tools/list must require the server's tools capability");

    assert!(matches!(refused, McpFederationError::Protocol(_)));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "no initialized notification or tools/list may follow a rejected handshake"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resource_and_prompt_only_server_is_a_valid_empty_tool_directory() {
    let state = behaviour(&[]);
    {
        let mut state = state.lock().unwrap();
        state.advertise_tools = false;
        state.advertise_resources = true;
        state.advertise_prompts = true;
    }
    let (endpoint, requests) = spawn_server(state).await;

    let catalog = client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .expect("resource/prompt-only servers are part of the MCP directory");

    assert!(catalog.tools.is_empty());
    assert_eq!(
        catalog.capabilities,
        BTreeSet::from([McpServerCapability::Resources, McpServerCapability::Prompts,])
    );
    assert_eq!(catalog.digest.len(), 64);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "the client must initialize and notify, but must not issue tools/list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_refuses_a_response_for_another_json_rpc_request() {
    let state = behaviour(&["web_search"]);
    state.lock().unwrap().response_id = 9;
    let (endpoint, requests) = spawn_server(state).await;

    let refused = client()
        .list_tools(Uuid::now_v7(), &open_server(endpoint))
        .await
        .expect_err("a response with the wrong JSON-RPC id must not be accepted");

    assert!(matches!(refused, McpFederationError::Protocol(_)));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
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
    let frozen = client().list_tools(tenant, &server).await.unwrap().digest;

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

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_call_session_must_still_advertise_tools_before_the_side_effect() {
    let state = behaviour(&["web_search"]);
    {
        let mut state = state.lock().unwrap();
        // Initial freeze and call-time re-discovery may observe Tools. The fresh
        // session opened for the actual side effect then drops that capability.
        state.advertise_tools_for_initializes = Some(2);
        state.advertise_resources = true;
    }
    let (endpoint, requests) = spawn_server(state).await;
    let server = open_server(endpoint);
    let tenant = Uuid::now_v7();
    let frozen = client().list_tools(tenant, &server).await.unwrap().digest;

    let refused = client()
        .call_tool(tenant, &server, "mcp:search/web_search", "{}", &frozen)
        .await
        .expect_err("the execution session dropped Tool authority");

    assert!(matches!(refused, McpFederationError::CatalogChanged));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        8,
        "the third initialize may be notified, but tools/call must not be sent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lifecycle_call_also_rechecks_tool_capability_before_the_side_effect() {
    let state = behaviour(&["web_search"]);
    {
        let mut state = state.lock().unwrap();
        state.advertise_tools_for_initializes = Some(2);
        state.advertise_prompts = true;
    }
    let (endpoint, requests) = spawn_server(state).await;
    let server = open_server(endpoint);
    let tenant = Uuid::now_v7();
    let client = client();
    let frozen = client.list_tools(tenant, &server).await.unwrap().digest;
    let (progress, _progress_receiver) = tokio::sync::mpsc::channel(1);
    let lifecycle = McpCallLifecycle {
        cancellation: tokio_util::sync::CancellationToken::new(),
        progress,
        progress_token: "capability-fence".into(),
        cancellation_reason: "test".into(),
    };

    let refused = client
        .call_tool_with_lifecycle(
            tenant,
            &server,
            "mcp:search/web_search",
            "{}",
            &frozen,
            &lifecycle,
        )
        .await
        .expect_err("the lifecycle execution session dropped Tool authority");

    assert!(matches!(refused, McpFederationError::CatalogChanged));
    assert_eq!(requests.load(Ordering::SeqCst), 8);
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
        McpFederationClient::from_pkcs8_pem(test_key_pem(), Duration::from_secs(5), true).unwrap();

    client
        .list_tools(
            tenant,
            &McpServerRef {
                server_id,
                name: "search".into(),
                endpoint,
                credential_envelope_json: envelope,
                oauth_credential_id: None,
                protocol_revision: McpProtocolRevision::V2025_06_18,
                client_capabilities: BTreeSet::new(),
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
        McpFederationClient::from_pkcs8_pem(test_key_pem(), Duration::from_secs(5), true).unwrap();

    let refused = client
        .list_tools(
            tenant,
            &McpServerRef {
                server_id: presented_as,
                name: "search".into(),
                endpoint,
                credential_envelope_json: envelope,
                oauth_credential_id: None,
                protocol_revision: McpProtocolRevision::V2025_06_18,
                client_capabilities: BTreeSet::new(),
            },
        )
        .await
        .expect_err("an envelope sealed for another server must not open");
    assert!(
        matches!(refused, McpFederationError::CredentialUnopenable),
        "expected CredentialUnopenable, got {refused:?}"
    );
}

/// A key too weak for model credentials must be too weak for MCP credentials.
/// A newer code path is not a reason to accept less.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_below_the_model_credential_floor_is_refused() {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};

    let weak = RsaPrivateKey::new(&mut OsRng, 2048)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();

    assert!(
        McpFederationClient::from_pkcs8_pem(&weak, Duration::from_secs(5), true).is_err(),
        "a 2048-bit key must be refused, as it is for model credentials"
    );
    assert!(
        McpFederationClient::from_pkcs8_pem(test_key_pem(), Duration::from_secs(0), true).is_err(),
        "a zero timeout must be refused"
    );
}
