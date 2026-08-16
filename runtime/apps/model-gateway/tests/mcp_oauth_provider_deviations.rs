//! Known real-world deviations from RFC 8414 / 9728 / 6749, exercised against
//! scripted loopback servers.
//!
//! The point is to learn where this client is too strict to interoperate and
//! where it is too lax to be safe. A scripted server is NOT evidence of real
//! external compatibility; it only pins down what we do when a provider behaves
//! a particular way.

use agent_model_gateway::mcp_oauth::{
    McpOAuthAuthorizationRequest, McpOAuthBinding, McpOAuthClientConfig, McpOAuthCoordinator,
    McpOAuthCredentialStatus, McpOAuthError,
};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use uuid::Uuid;

const MASTER_KEY: [u8; 32] = [31_u8; 32];

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("agent-mcp-oauth-dev-{}", Uuid::now_v7()));
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

struct ScriptedServer {
    origin: String,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedServer {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `(path prefix, status, content_type, body)`.
type Route = (&'static str, u16, &'static str, String);

async fn scripted_server(build: impl FnOnce(&str) -> Vec<Route>) -> ScriptedServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&hits);
    let routes = Arc::new(build(&origin));
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            let observed = Arc::clone(&observed);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4_096];
                let header_end = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let target = headers
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                observed.fetch_add(1, Ordering::SeqCst);
                let matched = routes.iter().find(|(path, ..)| target.starts_with(path));
                let response = match matched {
                    Some((_, status, content_type, body)) => format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_owned()
                    }
                };
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    ScriptedServer { origin, hits, task }
}

fn coordinator(root: &TestRoot) -> McpOAuthCoordinator {
    McpOAuthCoordinator::new(root.path(), MASTER_KEY, Duration::from_secs(5), true).unwrap()
}

fn binding(endpoint: &str) -> McpOAuthBinding {
    McpOAuthBinding {
        tenant_id: Uuid::now_v7(),
        server_id: Uuid::now_v7(),
        credential_id: Uuid::now_v7(),
        endpoint: endpoint.to_owned(),
    }
}

fn client_config() -> McpOAuthClientConfig {
    McpOAuthClientConfig {
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        requested_scopes: vec!["tools.read".to_owned()],
    }
}

const JSON: &str = "application/json";

/// Runs a token exchange against a scripted token body and reports what the
/// coordinator made of it.
async fn exchange_token_body(
    coordinator: &McpOAuthCoordinator,
    bound: &McpOAuthBinding,
    token_body: String,
) -> Result<(), McpOAuthError> {
    let provider = scripted_server(move |_| vec![("/token", 200, JSON, token_body)]).await;
    let request = McpOAuthAuthorizationRequest {
        authorization_endpoint: format!("{}/authorize", provider.origin),
        token_endpoint: format!("{}/token", provider.origin),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: None,
    };
    let start = coordinator
        .begin_authorization(bound.clone(), request, Utc::now())
        .await
        .expect("begin must succeed");
    let state = start
        .authorization_url
        .split("&state=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("authorization URL must carry state")
        .to_owned();
    coordinator
        .complete_authorization(
            bound.clone(),
            start.flow_id,
            &state,
            "callback-code",
            Utc::now(),
        )
        .await
        .map(drop)
}

/// Deviation 1: RFC 8414 wants the issuer to match exactly, but providers are
/// inconsistent about the trailing slash. Comparing parsed URLs rather than raw
/// strings makes the two spellings equal without accepting a different origin.
#[tokio::test]
async fn issuer_trailing_slash_mismatch_is_tolerated() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                // PRM names the issuer WITH a trailing slash...
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}/"],"scopes_supported":["tools.read"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                // ...while the server declares it WITHOUT one.
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let discovery = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("a trailing-slash difference must not fail discovery");
    assert_eq!(discovery.token_endpoint, format!("{}/token", server.origin));
}

/// Deviation 2: a Protected Resource Metadata document without
/// `scopes_supported` is legal; the authorization server's list is used instead.
#[tokio::test]
async fn protected_resource_metadata_without_scopes_falls_back_to_the_server() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"],"scopes_supported":["tools.read","tools.write"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let discovery = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("a PRM without scopes_supported is legal");
    assert!(
        discovery
            .scopes_supported
            .contains(&"tools.read".to_owned())
    );
}

/// Deviation 3: a provider that publishes no revocation endpoint must still be
/// revocable locally. Reporting an error would leave callers unable to revoke at
/// all against such a provider.
#[tokio::test]
async fn missing_revocation_endpoint_degrades_to_local_only() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":3600}"#
            .to_owned(),
    )
    .await
    .expect("exchange must succeed");

    let outcome = coordinator
        .revoke(bound.clone())
        .await
        .expect("revocation must not fail merely because the provider published no endpoint");
    assert!(!outcome.remote_confirmed);
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Revoked { .. }
    ));
}

/// Deviation 4: `expires_in` returned as a JSON string. RFC 6749 says number,
/// but string is common in the wild.
#[tokio::test]
async fn expires_in_as_a_string_is_accepted() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"a","token_type":"Bearer","expires_in":"3600"}"#.to_owned(),
    )
    .await
    .expect("a string expires_in must be accepted");
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Active { .. }
    ));
}

/// Deviation 5: `scope` returned as a JSON array instead of a space-delimited
/// string.
#[tokio::test]
async fn scope_as_an_array_is_accepted() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"a","token_type":"Bearer","expires_in":3600,"scope":["tools.read","tools.write"]}"#
            .to_owned(),
    )
    .await
    .expect("an array scope must be accepted");
}

/// Deviation 6: several authorization servers listed. Taking the first is a
/// deliberate choice, not an accident: every candidate would still have to pass
/// the same issuer and endpoint checks, and silently trying others would make
/// which server we trust depend on which one happened to answer.
#[tokio::test]
async fn multiple_authorization_servers_uses_the_first_only() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}","https://second.example"],"scopes_supported":["tools.read"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let discovery = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("the first authorization server must be used");
    assert_eq!(discovery.issuer, server.origin);
}

/// Deviation 7: unknown metadata fields must be ignored, not refused. A
/// conformant provider is allowed to publish more than we read.
#[tokio::test]
async fn unknown_metadata_fields_are_ignored() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"],"scopes_supported":["tools.read"],"bearer_methods_supported":["header"],"future_field":{{"nested":true}}}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256","plain"],"grant_types_supported":["authorization_code","refresh_token"],"ui_locales_supported":["en-US"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("unknown fields must be ignored");
}

/// Deviation 8: a challenge carrying several parameters alongside
/// `resource_metadata`.
#[tokio::test]
async fn challenge_with_additional_parameters_is_parsed() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"],"scopes_supported":["tools.read"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let challenge = format!(
        r#"Bearer realm="mcp", error="invalid_token", error_description="expired", resource_metadata="{}/.well-known/oauth-protected-resource""#,
        server.origin
    );
    coordinator(&root)
        .discover(&binding(&endpoint), Some(&challenge))
        .await
        .expect("extra challenge parameters must not defeat parsing");
}

/// Deviation 9: a token endpoint answering 200 with something that is not JSON.
/// This must fail rather than produce a credential.
#[tokio::test]
async fn non_json_token_body_is_refused() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let error = exchange_token_body(
        &coordinator,
        &bound,
        "access_token=a&token_type=bearer".to_owned(),
    )
    .await
    .expect_err("a form-encoded token body must not be accepted as JSON");
    assert!(matches!(error, McpOAuthError::ProviderUnavailable));
    assert!(!matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Active { .. }
    ));
}

/// Deviation 10: metadata served with a non-JSON content type. Parsing is the
/// real gate; the header is advisory and some providers get it wrong. Accepting
/// it costs nothing, because a body that does not parse is still refused.
#[tokio::test]
async fn metadata_content_type_is_advisory() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                "text/plain",
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"],"scopes_supported":["tools.read"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                "text/plain",
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("a wrong content type must not defeat a parseable document");
    assert!(server.hits() >= 2);
}

/// Deviation 11: the authorization server on a different origin than the
/// resource. This is the common production shape -- api.example.com protected by
/// auth.example.com -- so refusing it would be a serious over-strictness. Only
/// the challenge-named metadata URL is same-origin constrained; the issuer the
/// document points to is not.
#[tokio::test]
async fn authorization_server_on_a_different_origin_is_permitted() {
    let root = TestRoot::new();
    let authorization = scripted_server(|origin| {
        vec![(
            "/.well-known/oauth-authorization-server",
            200,
            JSON,
            format!(
                r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"],"scopes_supported":["tools.read"]}}"#
            ),
        )]
    })
    .await;
    let issuer = authorization.origin.clone();
    let resource = scripted_server(move |origin| {
        vec![(
            "/.well-known/oauth-protected-resource",
            200,
            JSON,
            format!(
                r#"{{"resource":"{origin}/mcp","authorization_servers":["{issuer}"],"scopes_supported":["tools.read"]}}"#
            ),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", resource.origin);

    let discovery = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect("a separate authorization server origin is the common production shape");
    assert_eq!(discovery.issuer, authorization.origin);
    assert_eq!(
        discovery.token_endpoint,
        format!("{}/token", authorization.origin)
    );
    assert!(resource.hits() >= 1 && authorization.hits() >= 1);
}

/// Deviation 12: a token endpoint that answers HTTP 200 with an OAuth error
/// body. RFC 6749 requires 4xx, but this happens. It must not yield a
/// credential.
#[tokio::test]
async fn token_error_carried_on_http_200_does_not_create_a_credential() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let error = exchange_token_body(
        &coordinator,
        &bound,
        r#"{"error":"invalid_grant","error_description":"code already used"}"#.to_owned(),
    )
    .await
    .expect_err("an error body must not produce a credential even at HTTP 200");
    assert!(matches!(error, McpOAuthError::ProviderUnavailable));
    assert!(!matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Active { .. }
    ));
}

/// Deviation 13: an empty access token at HTTP 200. Structurally valid JSON,
/// semantically useless, and it must not be stored as a working credential.
#[tokio::test]
async fn an_empty_access_token_is_refused() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let error = exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"","token_type":"Bearer","expires_in":3600}"#.to_owned(),
    )
    .await
    .expect_err("an empty access token must be refused");
    assert!(matches!(error, McpOAuthError::ProviderUnavailable));
}

/// Deviation 14: a metadata response whose body is shorter than its declared
/// Content-Length, then the connection closes. A truncated document must fail
/// rather than be parsed as whatever arrived.
#[tokio::test]
async fn a_truncated_metadata_stream_is_not_a_false_success() {
    let root = TestRoot::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut chunk = [0_u8; 4_096];
                let _ = socket.read(&mut chunk).await;
                // Declares far more than it sends, then hangs up.
                let partial = r#"{"resource":"http://example"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n{partial}"
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    let endpoint = format!("{origin}/mcp");
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("a truncated metadata body must not be accepted");
    assert!(matches!(
        error,
        McpOAuthError::DiscoveryRejected | McpOAuthError::ProviderUnavailable
    ));
    task.abort();
}

/// A token server that records every `refresh_token` it is presented with, so a
/// test can assert which one the second refresh actually used.
struct RefreshRecorder {
    origin: String,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RefreshRecorder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `rotate` decides whether each refresh response carries a NEW refresh token or
/// omits the field entirely, which is the difference between a rotating and a
/// non-rotating provider.
async fn refresh_recorder(rotate: bool) -> RefreshRecorder {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        let mut round = 0_usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed = Arc::clone(&observed);
            round += 1;
            let index = round;
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4_096];
                let header_end = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let body = String::from_utf8_lossy(&bytes[header_end..]).into_owned();
                if let Some(rest) = body.split("refresh_token=").nth(1) {
                    let value = rest.split('&').next().unwrap_or("").to_owned();
                    observed.lock().unwrap().push(value);
                }
                // Every token stays immediately expired so the next resolve is
                // forced to refresh again.
                let payload = if body.contains("grant_type=authorization_code") {
                    r#"{"access_token":"access-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":0}"#.to_owned()
                } else if rotate {
                    format!(
                        r#"{{"access_token":"access-r{index}","refresh_token":"rotated-{index}","token_type":"Bearer","expires_in":0}}"#
                    )
                } else {
                    format!(
                        r#"{{"access_token":"access-r{index}","token_type":"Bearer","expires_in":0}}"#
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    RefreshRecorder { origin, seen, task }
}

async fn activate_against(
    coordinator: &McpOAuthCoordinator,
    bound: &McpOAuthBinding,
    origin: &str,
) {
    let request = McpOAuthAuthorizationRequest {
        authorization_endpoint: format!("{origin}/authorize"),
        token_endpoint: format!("{origin}/token"),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: None,
    };
    let start = coordinator
        .begin_authorization(bound.clone(), request, Utc::now())
        .await
        .expect("begin must succeed");
    let state = start
        .authorization_url
        .split("&state=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("state")
        .to_owned();
    coordinator
        .complete_authorization(
            bound.clone(),
            start.flow_id,
            &state,
            "callback-code",
            Utc::now(),
        )
        .await
        .expect("exchange must succeed");
}

/// Deviation 15: a rotating provider issues a new refresh token on every
/// refresh. The next refresh must present the new one; presenting the original
/// again would fail against any provider that invalidates on rotation.
#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_previous_one() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = refresh_recorder(true).await;
    activate_against(&coordinator, &bound, &provider.origin).await;

    // Two resolves, each forced to refresh because every token expires at once.
    let _ = coordinator
        .resolve_access_token(bound.clone(), Utc::now())
        .await;
    let _ = coordinator.resolve_access_token(bound, Utc::now()).await;

    let seen = provider.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "expected two refreshes, saw {seen:?}");
    assert_eq!(seen[0], "refresh-1", "the first refresh uses the original");
    assert!(
        seen[1].starts_with("rotated-"),
        "the second refresh must present the rotated token, saw {:?}",
        seen[1]
    );
}

/// Deviation 16: a non-rotating provider omits `refresh_token` on refresh. The
/// original must be carried forward -- dropping it would strand the credential
/// after one refresh.
#[tokio::test]
async fn a_provider_that_omits_refresh_token_keeps_the_original() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = refresh_recorder(false).await;
    activate_against(&coordinator, &bound, &provider.origin).await;

    let _ = coordinator
        .resolve_access_token(bound.clone(), Utc::now())
        .await;
    let _ = coordinator.resolve_access_token(bound, Utc::now()).await;

    let seen = provider.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "expected two refreshes, saw {seen:?}");
    assert!(
        seen.iter().all(|token| token == "refresh-1"),
        "a provider that omits the field must keep the original, saw {seen:?}"
    );
}

/// Deviation 17: the provider grants a narrower scope than was requested. That
/// is its prerogative under RFC 6749 and must not be treated as a failure.
#[tokio::test]
async fn a_narrower_granted_scope_is_accepted() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    // Requested tools.read; granted only a lesser scope.
    exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"a","token_type":"Bearer","expires_in":3600,"scope":"tools.list"}"#
            .to_owned(),
    )
    .await
    .expect("a narrower granted scope is the provider's prerogative, not a failure");
    // Accepting it is not enough: the narrowing has to be visible, or the first
    // symptom of a quiet downgrade is a tool call failing for no stated reason.
    match coordinator.status(bound).await.unwrap() {
        McpOAuthCredentialStatus::Active { granted_scopes, .. } => assert_eq!(
            granted_scopes,
            vec!["tools.list".to_owned()],
            "the status must report what was granted, not what was requested"
        ),
        other => panic!("expected an active credential, got {other:?}"),
    }
}

/// Deviation 18: a non-Bearer token type. We only know how to present Bearer, so
/// storing a `mac` token would produce a credential that silently fails every
/// request.
#[tokio::test]
async fn a_non_bearer_token_type_is_refused() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let error = exchange_token_body(
        &coordinator,
        &bound,
        r#"{"access_token":"a","token_type":"mac","expires_in":3600}"#.to_owned(),
    )
    .await
    .expect_err("a token type we cannot present must be refused");
    assert!(matches!(error, McpOAuthError::ProviderUnavailable));
    assert!(!matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Active { .. }
    ));
}

/// Deviation 19: two authorizations opened against the same credential. The
/// second must supersede the first, and the first flow's state must no longer be
/// redeemable -- otherwise an abandoned flow stays a live way in.
#[tokio::test]
async fn a_second_begin_invalidates_the_first_flow() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = scripted_server(|_| {
        vec![(
            "/token",
            200,
            JSON,
            r#"{"access_token":"a","token_type":"Bearer","expires_in":3600}"#.to_owned(),
        )]
    })
    .await;
    let request = || McpOAuthAuthorizationRequest {
        authorization_endpoint: format!("{}/authorize", provider.origin),
        token_endpoint: format!("{}/token", provider.origin),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: None,
    };
    let extract_state = |url: &str| {
        url.split("&state=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("state")
            .to_owned()
    };

    let first = coordinator
        .begin_authorization(bound.clone(), request(), Utc::now())
        .await
        .unwrap();
    let first_state = extract_state(&first.authorization_url);
    let second = coordinator
        .begin_authorization(bound.clone(), request(), Utc::now())
        .await
        .unwrap();
    assert_ne!(first.flow_id, second.flow_id);

    // The abandoned flow must be dead, both by its id and by its state.
    let stale = match coordinator
        .complete_authorization(
            bound.clone(),
            first.flow_id,
            &first_state,
            "callback-code",
            Utc::now(),
        )
        .await
    {
        Ok(_) => panic!("a superseded flow must not be redeemable"),
        Err(error) => error,
    };
    assert!(matches!(stale, McpOAuthError::InvalidAuthorizationCallback));

    // The surviving flow still works.
    let second_state = extract_state(&second.authorization_url);
    coordinator
        .complete_authorization(
            bound.clone(),
            second.flow_id,
            &second_state,
            "callback-code",
            Utc::now(),
        )
        .await
        .expect("the most recent flow must still be redeemable");
}

/// Deviation 20: a token whose lifetime lands inside the refresh skew window.
/// Presenting it would be a request we already expect to fail, so it must be
/// refreshed on resolve rather than used.
#[tokio::test]
async fn a_token_expiring_inside_the_skew_window_is_refreshed_not_used() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = refresh_recorder(true).await;

    // The exchange issues a token with expires_in 0, which is inside the skew
    // window, so the very first resolve must refresh rather than present it.
    activate_against(&coordinator, &bound, &provider.origin).await;
    let _ = coordinator
        .resolve_access_token(bound.clone(), Utc::now())
        .await;

    let seen = provider.seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "resolve must have refreshed exactly once, saw {seen:?}"
    );
    assert_eq!(seen[0], "refresh-1");
}

/// Deviation 21: an authorization endpoint that already carries query
/// parameters. Some providers publish a tenant or audience discriminator there,
/// and dropping it would send the user to the wrong authorization context while
/// every other parameter still looked right.
#[tokio::test]
async fn an_authorization_endpoint_with_existing_query_is_preserved() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let request = McpOAuthAuthorizationRequest {
        authorization_endpoint: "http://127.0.0.1:9/authorize?tenant=acme&audience=mcp".to_owned(),
        token_endpoint: "http://127.0.0.1:9/token".to_owned(),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: None,
    };
    let start = coordinator
        .begin_authorization(bound, request, Utc::now())
        .await
        .expect("an endpoint with an existing query must be usable");

    let url = start.authorization_url;
    assert!(
        url.contains("tenant=acme"),
        "the provider's own query parameters must survive: {url}"
    );
    assert!(
        url.contains("audience=mcp"),
        "the provider's own query parameters must survive: {url}"
    );
    assert!(url.contains("response_type=code"), "{url}");
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    // Exactly one '?': the added parameters must extend the existing query
    // rather than start a second one.
    assert_eq!(
        url.matches('?').count(),
        1,
        "a second question mark would make the query unparseable: {url}"
    );
}

/// Deviation 22: RFC 8707 `resource`. We do not send it, so a provider that
/// requires it will refuse the exchange. Recorded as a known incompatibility
/// rather than guessed at: sending a resource indicator that the authorization
/// server does not expect can itself change which audience is issued.
#[tokio::test]
async fn the_authorization_url_does_not_claim_an_rfc8707_resource() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let request = McpOAuthAuthorizationRequest {
        authorization_endpoint: "http://127.0.0.1:9/authorize".to_owned(),
        token_endpoint: "http://127.0.0.1:9/token".to_owned(),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: None,
    };
    let start = coordinator
        .begin_authorization(bound, request, Utc::now())
        .await
        .unwrap();
    assert!(
        !start.authorization_url.contains("resource="),
        "documenting current behaviour: no resource indicator is sent"
    );
}

/// Revocation must be terminal under concurrency.
///
/// The dangerous interleaving is a refresh that was already in flight when a
/// revoke commits: if its result landed afterwards it would resurrect a
/// credential the tenant believes is dead, and every later request would succeed
/// against a provider the operator thought they had cut off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoke_cannot_be_resurrected_by_a_concurrent_refresh() {
    let root = TestRoot::new();
    let coordinator = Arc::new(coordinator(&root));
    let bound = binding("http://127.0.0.1:9/mcp");
    // Every token is issued already expired, so each resolve is forced into a
    // refresh and the window this test cares about is always open.
    let provider = refresh_recorder(true).await;
    activate_against(&coordinator, &bound, &provider.origin).await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let coordinator = Arc::clone(&coordinator);
        let bound = bound.clone();
        tasks.push(tokio::spawn(async move {
            let _ = coordinator.resolve_access_token(bound, Utc::now()).await;
        }));
    }
    let revoker = {
        let coordinator = Arc::clone(&coordinator);
        let bound = bound.clone();
        tokio::spawn(async move { coordinator.revoke(bound).await })
    };
    for task in tasks {
        task.await.unwrap();
    }
    revoker.await.unwrap().expect("revoke must succeed");

    // Whatever the interleaving, the credential must end dead and stay dead.
    assert!(
        matches!(
            coordinator.status(bound.clone()).await.unwrap(),
            McpOAuthCredentialStatus::Revoked { .. }
        ),
        "a revoked credential must not be resurrected by an in-flight refresh"
    );
    assert!(
        coordinator
            .resolve_access_token(bound, Utc::now())
            .await
            .is_err(),
        "nothing may resolve after revocation"
    );
}

/// Recording a rejection concurrently with refreshes must never move a
/// credential the refreshes have already superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rejections_cannot_unseat_a_newer_revision() {
    let root = TestRoot::new();
    let coordinator = Arc::new(coordinator(&root));
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = refresh_recorder(true).await;
    activate_against(&coordinator, &bound, &provider.origin).await;

    // Eight stale digests racing eight refreshes. A stale digest is one no
    // revision ever had, so none of them may take effect no matter when they
    // land.
    let mut tasks = Vec::new();
    for index in 0..8 {
        let coordinator = Arc::clone(&coordinator);
        let bound = bound.clone();
        tasks.push(tokio::spawn(async move {
            let _ = coordinator
                .resolve_access_token(bound.clone(), Utc::now())
                .await;
            coordinator
                .record_rejected_access_token(bound, &format!("{index:064}"))
                .await
        }));
    }
    for task in tasks {
        let accepted = task.await.unwrap().expect("recording must not error");
        assert!(
            !accepted,
            "a digest no revision ever held must never be accepted"
        );
    }
}

/// The strictness that must NOT be relaxed for compatibility: a provider whose
/// metadata omits S256 does not get to fall back to `plain`.
#[tokio::test]
async fn missing_s256_is_still_refused_after_leniency() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                JSON,
                format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"],"scopes_supported":["tools.read"]}}"#
                ),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                JSON,
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["plain"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("a plain-only provider must still be refused");
    assert!(matches!(error, McpOAuthError::DiscoveryRejected));
    let _ = client_config();
}
