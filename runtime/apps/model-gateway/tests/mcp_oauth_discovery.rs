//! MCP OAuth stage two (ADR-0119): standard discovery and rejection feedback.
//!
//! These tests drive real loopback HTTP. A case that never issues a request, or
//! that would pass against a server that was never started, is a false green and
//! is asserted against explicitly via request counters.

use agent_model_gateway::mcp::{McpFederationClient, McpFederationError, McpServerRef};
use agent_model_gateway::mcp_oauth::{
    McpOAuthAuthorizationReason, McpOAuthAuthorizationRequest, McpOAuthBinding,
    McpOAuthClientConfig, McpOAuthCoordinator, McpOAuthCredentialStatus, McpOAuthError,
};
use agent_protocol::McpProtocolRevision;
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use uuid::Uuid;

const MASTER_KEY: [u8; 32] = [7_u8; 32];

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("agent-mcp-oauth-disc-{}", Uuid::now_v7()));
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

/// A scripted HTTP origin. Each route is matched on the request target prefix so
/// a test can assert exactly which discovery documents were fetched.
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

/// Routes are `(path, status, extra_headers, body)`.
type Route = (&'static str, u16, &'static str, String);

/// Routes are built from the bound origin so a document can name its own
/// address. Binding first and scripting second is what makes that possible.
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
                    Some((_, status, extra, body)) => format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
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

fn prm(resource: &str, issuer: &str) -> String {
    format!(
        r#"{{"resource":"{resource}","authorization_servers":["{issuer}"],"scopes_supported":["tools.read"]}}"#
    )
}

fn as_metadata(issuer: &str) -> String {
    format!(
        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"],"scopes_supported":["tools.read"]}}"#
    )
}

/// Earns an active credential through the real begin -> exchange path. The
/// production type deliberately exposes no credential-injection seam, so the
/// tests reach `Active` the same way a deployment does.
async fn activate_via_real_exchange(
    coordinator: &McpOAuthCoordinator,
    bound: &McpOAuthBinding,
    access_token: &str,
    refresh_token: Option<&str>,
) -> String {
    let refresh = refresh_token
        .map(|token| format!(r#","refresh_token":"{token}""#))
        .unwrap_or_default();
    let body = format!(
        r#"{{"access_token":"{access_token}","token_type":"Bearer","expires_in":3600{refresh}}}"#
    );
    let provider = scripted_server(move |_| vec![("/token", 200, "", body)]).await;
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
        .expect("begin must succeed against the scripted provider");
    // The state is URL-safe base64 with no padding, so it needs no decoding.
    let state = start
        .authorization_url
        .split("&state=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("authorization URL must carry state")
        .to_owned();
    let resolved = coordinator
        .complete_authorization(
            bound.clone(),
            start.flow_id,
            &state,
            "callback-code",
            Utc::now(),
        )
        .await
        .expect("exchange must succeed against the scripted provider");
    assert_eq!(provider.hits(), 1, "exactly one token request");
    resolved.token_digest().to_owned()
}

/// Activates a credential whose provider also publishes a revocation endpoint,
/// so `revoke` has somewhere real to call. The server is returned so the caller
/// keeps it alive and can assert on its hit count.
async fn activate_with_revocation(
    coordinator: &McpOAuthCoordinator,
    bound: &McpOAuthBinding,
    revocation_status: u16,
) -> ScriptedServer {
    let provider = scripted_server(move |_| {
        vec![
            (
                "/token",
                200,
                "",
                r#"{"access_token":"live-token","refresh_token":"refresh-value","token_type":"Bearer","expires_in":3600}"#
                    .to_owned(),
            ),
            ("/revoke", revocation_status, "", "{}".to_owned()),
        ]
    })
    .await;
    let request = McpOAuthAuthorizationRequest {
        authorization_endpoint: format!("{}/authorize", provider.origin),
        token_endpoint: format!("{}/token", provider.origin),
        client_id: "trusted-public-client".to_owned(),
        redirect_uri: "http://127.0.0.1:53535/callback".to_owned(),
        scopes: vec!["tools.read".to_owned()],
        revocation_endpoint: Some(format!("{}/revoke", provider.origin)),
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
        .expect("exchange must succeed");
    provider
}

/// Local revocation commits before the provider is contacted, so a provider
/// that refuses -- or is hostile, or simply down -- cannot leave the credential
/// usable here.
#[tokio::test]
async fn remote_revocation_failure_still_revokes_locally() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = activate_with_revocation(&coordinator, &bound, 500).await;

    let outcome = coordinator.revoke(bound.clone()).await.unwrap();
    assert!(
        !outcome.remote_confirmed,
        "a 500 from the provider must never be reported as confirmed"
    );
    assert!(matches!(
        coordinator.status(bound.clone()).await.unwrap(),
        McpOAuthCredentialStatus::Revoked { .. }
    ));
    assert!(
        coordinator
            .resolve_access_token(bound, Utc::now())
            .await
            .is_err(),
        "a revoked credential must not resolve"
    );
    assert!(
        provider.hits() >= 2,
        "the token exchange and the revocation must both be real requests, saw {}",
        provider.hits()
    );
}

/// A provider that accepts the revocation is reported as confirmed.
#[tokio::test]
async fn remote_revocation_success_is_reported() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let provider = activate_with_revocation(&coordinator, &bound, 200).await;

    let outcome = coordinator.revoke(bound.clone()).await.unwrap();
    assert!(outcome.remote_confirmed);
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::Revoked { .. }
    ));
    assert!(provider.hits() >= 2);
}

/// Case 1: WWW-Authenticate challenge -> Protected Resource Metadata ->
/// Authorization Server Metadata -> S256 PKCE authorization URL.
#[tokio::test]
async fn challenge_drives_protected_resource_then_authorization_server_metadata() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                "",
                prm(&format!("{origin}/mcp"), origin),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                "",
                as_metadata(origin),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let coordinator = coordinator(&root);
    let bound = binding(&endpoint);

    let challenge = format!(
        r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#,
        server.origin
    );
    let discovery = coordinator
        .discover(&bound, Some(&challenge))
        .await
        .expect("discovery must succeed against a live metadata origin");

    assert_eq!(
        discovery.resource, endpoint,
        "resource binds the MCP endpoint"
    );
    assert_eq!(discovery.issuer, server.origin);
    assert_eq!(
        discovery.authorization_endpoint,
        format!("{}/authorize", server.origin)
    );
    assert_eq!(discovery.token_endpoint, format!("{}/token", server.origin));
    assert!(
        server.hits() >= 2,
        "both metadata documents must be fetched over real HTTP, saw {}",
        server.hits()
    );

    let start = coordinator
        .begin_discovered_authorization(bound.clone(), client_config(), None, Utc::now())
        .await
        .expect("PKCE begin must succeed from discovered metadata");
    assert!(
        start
            .authorization_url
            .contains("code_challenge_method=S256")
    );
    assert!(start.authorization_url.contains("response_type=code"));
    assert!(!start.authorization_url.contains("response_type=token"));
    assert!(!start.authorization_url.contains("code_verifier"));
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::PendingAuthorization { .. }
    ));
}

/// Case 2: metadata whose `resource` names a different origin than the MCP
/// endpoint is a substitution attempt and must fail closed.
#[tokio::test]
async fn resource_mismatch_is_refused() {
    let root = TestRoot::new();
    let server = scripted_server(|_| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                "",
                prm("https://attacker.example/mcp", "https://attacker.example"),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                "",
                as_metadata("https://attacker.example"),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let coordinator = coordinator(&root);

    let error = coordinator
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("resource pointing at another origin must be refused");
    assert!(matches!(error, McpOAuthError::DiscoveryRejected));
    assert!(server.hits() >= 1, "the document must actually be fetched");
}

/// Case 2b: an authorization server whose `issuer` disagrees with its own
/// endpoints is inconsistent and must fail closed.
#[tokio::test]
async fn issuer_endpoint_disagreement_is_refused() {
    let root = TestRoot::new();
    let server = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                "",
                prm(&format!("{origin}/mcp"), origin),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                "",
                format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"https://elsewhere.example/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"]}}"#
                ),
            ),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("authorization endpoint outside the issuer must be refused");
    assert!(matches!(error, McpOAuthError::DiscoveryRejected));
}

/// Case 3a: a metadata body beyond 64 KiB must be refused rather than buffered.
#[tokio::test]
async fn oversized_metadata_body_is_refused() {
    let root = TestRoot::new();
    let filler = "a".repeat(70 * 1024);
    let server = scripted_server(move |_| {
        vec![(
            "/.well-known/oauth-protected-resource",
            200,
            "",
            format!(r#"{{"resource":"x","padding":"{filler}"}}"#),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("a body beyond the cap must be refused");
    assert!(matches!(
        error,
        McpOAuthError::DiscoveryRejected | McpOAuthError::ProviderUnavailable
    ));
}

/// Case 3b: a single field beyond 4 KiB must be refused.
#[tokio::test]
async fn oversized_metadata_field_is_refused() {
    let root = TestRoot::new();
    let long_issuer = format!("https://{}.example", "b".repeat(5 * 1024));
    let server = scripted_server(move |_| {
        vec![(
            "/.well-known/oauth-protected-resource",
            200,
            "",
            prm("x", &long_issuer),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("a field beyond the cap must be refused");
    assert!(matches!(error, McpOAuthError::DiscoveryRejected));
}

/// Case 3c: discovery must not follow redirects to another origin.
#[tokio::test]
async fn metadata_redirect_is_not_followed() {
    let root = TestRoot::new();
    let server = scripted_server(|_| {
        vec![(
            "/.well-known/oauth-protected-resource",
            302,
            "location: https://attacker.example/prm\r\n",
            String::new(),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(&binding(&endpoint), None)
        .await
        .expect_err("a redirect must not be followed");
    assert!(matches!(
        error,
        McpOAuthError::DiscoveryRejected | McpOAuthError::ProviderUnavailable
    ));
}

/// Case 3d: a challenge naming a metadata URL on a different origin than the
/// MCP endpoint must be refused before any request is issued.
#[tokio::test]
async fn challenge_metadata_url_on_foreign_origin_is_refused() {
    let root = TestRoot::new();
    let server = scripted_server(|_| {
        vec![(
            "/.well-known/oauth-protected-resource",
            200,
            "",
            prm("x", "y"),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let error = coordinator(&root)
        .discover(
            &binding(&endpoint),
            Some(r#"Bearer resource_metadata="https://attacker.example/prm""#),
        )
        .await
        .expect_err("a foreign metadata origin must be refused");
    assert!(matches!(error, McpOAuthError::DiscoveryRejected));
    assert_eq!(
        server.hits(),
        0,
        "a foreign metadata origin must be refused before any request"
    );
}

/// Case 3e: a malformed or oversized challenge must fail closed rather than
/// falling back to an unauthenticated default.
#[tokio::test]
async fn malformed_challenge_fails_closed() {
    let root = TestRoot::new();
    let server = scripted_server(|_| {
        vec![(
            "/.well-known/oauth-protected-resource",
            200,
            "",
            prm("x", "y"),
        )]
    })
    .await;
    let endpoint = format!("{}/mcp", server.origin);
    let coordinator = coordinator(&root);
    for challenge in [
        "Bearer resource_metadata=\"\x00\x01\"",
        &format!("Bearer resource_metadata=\"{}\"", "z".repeat(9_000)),
        "Bearer resource_metadata=\"file:///etc/passwd\"",
        "Bearer resource_metadata=\"http://169.254.169.254/prm\"",
    ] {
        let error = coordinator
            .discover(&binding(&endpoint), Some(challenge))
            .await
            .expect_err("malformed challenge must fail closed");
        assert!(
            matches!(error, McpOAuthError::DiscoveryRejected),
            "unexpected error for challenge {challenge:?}"
        );
    }
}

/// Case 7: a pending flow freezes its discovery result. Swapping the served
/// metadata after `begin` must not change where the callback exchanges the code.
#[tokio::test]
async fn pending_flow_rejects_substituted_metadata_at_callback() {
    let root = TestRoot::new();
    let honest = scripted_server(|origin| {
        vec![
            (
                "/.well-known/oauth-protected-resource",
                200,
                "",
                prm(&format!("{origin}/mcp"), origin),
            ),
            (
                "/.well-known/oauth-authorization-server",
                200,
                "",
                as_metadata(origin),
            ),
            // The frozen token endpoint refuses, proving the callback used it.
            ("/token", 400, "", r#"{"error":"invalid_grant"}"#.to_owned()),
        ]
    })
    .await;
    let endpoint = format!("{}/mcp", honest.origin);
    let coordinator = coordinator(&root);
    let bound = binding(&endpoint);
    coordinator.discover(&bound, None).await.unwrap();
    let start = coordinator
        .begin_discovered_authorization(bound.clone(), client_config(), None, Utc::now())
        .await
        .unwrap();

    // A callback carrying a valid flow id but a forged state must be refused,
    // and the frozen endpoints must remain authoritative.
    // `expect_err` is unavailable on purpose: the success type holds a token and
    // deliberately implements neither Debug nor Clone.
    let forged = match coordinator
        .complete_authorization(
            bound.clone(),
            start.flow_id,
            "forged-state",
            "code",
            Utc::now(),
        )
        .await
    {
        Ok(_) => panic!("a forged state must be refused"),
        Err(error) => error,
    };
    assert!(matches!(
        forged,
        McpOAuthError::InvalidAuthorizationCallback
    ));
}

/// An MCP endpoint that always answers with one scripted status. The hit
/// counter is what proves the absence of a replay: an assertion that the call
/// "failed" would pass even if the client had quietly retried first.
async fn fixed_status_mcp_server(
    status_line: &'static str,
    extra_headers: &'static str,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&hits);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed = Arc::clone(&observed);
            tokio::spawn(async move {
                let mut chunk = [0_u8; 4_096];
                if socket.read(&mut chunk).await.unwrap_or(0) == 0 {
                    return;
                }
                observed.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"denied"}"#;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\n{extra_headers}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    (endpoint, hits, task)
}

fn oauth_server_ref(bound: &McpOAuthBinding) -> McpServerRef {
    McpServerRef {
        server_id: bound.server_id,
        name: "oauth".into(),
        endpoint: bound.endpoint.clone(),
        credential_envelope_json: String::new(),
        oauth_credential_id: Some(bound.credential_id),
        protocol_revision: McpProtocolRevision::V2026_07_28,
        client_capabilities: BTreeSet::new(),
    }
}

/// Case 4 + 6, end to end through the real transport: a live 401 against the
/// *current* token moves the credential to AuthorizationRequired, and the call
/// is not replayed.
#[tokio::test]
async fn live_401_marks_current_token_rejected_without_replay() {
    let root = TestRoot::new();
    let (endpoint, hits, task) = fixed_status_mcp_server(
        "401 Unauthorized",
        "www-authenticate: Bearer error=\"invalid_token\"\r\n",
    )
    .await;
    let coordinator = coordinator(&root);
    let bound = binding(&endpoint);
    activate_via_real_exchange(&coordinator, &bound, "live-token", Some("refresh-value")).await;
    assert!(
        matches!(
            coordinator.status(bound.clone()).await.unwrap(),
            McpOAuthCredentialStatus::Active { .. }
        ),
        "the credential must start active, or the test proves nothing"
    );

    let coordinator = Arc::new(coordinator);
    let client = McpFederationClient::for_open_servers(Duration::from_secs(5), true)
        .unwrap()
        .with_oauth_coordinator(Arc::clone(&coordinator));
    let error = match client
        .list_tools(bound.tenant_id, &oauth_server_ref(&bound))
        .await
    {
        Ok(_) => panic!("a 401 must not produce a catalog"),
        Err(error) => error,
    };
    assert!(
        matches!(error, McpFederationError::AuthorizationRequired),
        "a 401 must surface as AuthorizationRequired, not as an outage: {error}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "an authentication failure must not be replayed"
    );
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::AuthorizationRequired {
            reason: McpOAuthAuthorizationReason::AccessTokenRejected,
            ..
        }
    ));
    task.abort();
}

/// A 401 wearing an event-stream content type must classify the same as a 401
/// wearing a JSON one.
///
/// The status is checked before the transport branch is chosen, so this should
/// hold -- but "should" is exactly the kind of claim that stops being true when
/// someone reorders those two steps, and nothing else would notice.
#[tokio::test]
async fn a_401_on_the_event_stream_path_is_still_a_token_rejection() {
    let root = TestRoot::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&hits);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let observed = Arc::clone(&observed);
            tokio::spawn(async move {
                let mut chunk = [0_u8; 4_096];
                if socket.read(&mut chunk).await.unwrap_or(0) == 0 {
                    return;
                }
                observed.fetch_add(1, Ordering::SeqCst);
                let body = "event: message\ndata: {}\n\n";
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer error=\"invalid_token\"\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });

    let coordinator = coordinator(&root);
    let bound = binding(&endpoint);
    activate_via_real_exchange(&coordinator, &bound, "live-token", Some("refresh-value")).await;

    let coordinator = Arc::new(coordinator);
    let client = McpFederationClient::for_open_servers(Duration::from_secs(5), true)
        .unwrap()
        .with_oauth_coordinator(Arc::clone(&coordinator));
    let error = match client
        .list_tools(bound.tenant_id, &oauth_server_ref(&bound))
        .await
    {
        Ok(_) => panic!("a 401 must not produce a catalog"),
        Err(error) => error,
    };
    assert!(
        matches!(error, McpFederationError::AuthorizationRequired),
        "an event-stream 401 must classify as a rejection, not an outage: {error}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no replay");
    assert!(matches!(
        coordinator.status(bound).await.unwrap(),
        McpOAuthCredentialStatus::AuthorizationRequired {
            reason: McpOAuthAuthorizationReason::AccessTokenRejected,
            ..
        }
    ));
    task.abort();
}

/// A 403 is an authorization decision about a caller that authenticated fine.
/// Recording it as a dead token would let one permission change force every
/// tenant through re-authorization.
#[tokio::test]
async fn live_403_does_not_mark_the_token_rejected() {
    let root = TestRoot::new();
    let (endpoint, hits, task) = fixed_status_mcp_server("403 Forbidden", "").await;
    let coordinator = coordinator(&root);
    let bound = binding(&endpoint);
    activate_via_real_exchange(&coordinator, &bound, "live-token", Some("refresh-value")).await;

    let coordinator = Arc::new(coordinator);
    let client = McpFederationClient::for_open_servers(Duration::from_secs(5), true)
        .unwrap()
        .with_oauth_coordinator(Arc::clone(&coordinator));
    assert!(
        client
            .list_tools(bound.tenant_id, &oauth_server_ref(&bound))
            .await
            .is_err()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        matches!(
            coordinator.status(bound).await.unwrap(),
            McpOAuthCredentialStatus::Active { .. }
        ),
        "a 403 must leave the credential active"
    );
    task.abort();
}

/// Case 5: a digest that is no longer current -- a late 401 carrying a token a
/// concurrent refresh already superseded -- must not re-drive the state machine.
#[tokio::test]
async fn stale_digest_cannot_override_committed_state() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    let bound = binding("http://127.0.0.1:9/mcp");
    let digest = activate_via_real_exchange(&coordinator, &bound, "live-token", None).await;

    assert!(
        coordinator
            .record_rejected_access_token(bound.clone(), &digest)
            .await
            .unwrap(),
        "the current token digest must be accepted"
    );
    assert!(
        !coordinator
            .record_rejected_access_token(bound, &"0".repeat(64))
            .await
            .unwrap(),
        "a stale digest must not change committed state"
    );
}

/// Case 8: no token, code, verifier or state may appear in the persisted files.
#[tokio::test]
async fn persisted_state_never_contains_plaintext_secrets() {
    let root = TestRoot::new();
    let coordinator = coordinator(&root);
    // A loopback literal keeps the endpoint policy satisfied without depending
    // on external DNS; the token exchange below is still real HTTP.
    let bound = binding("http://127.0.0.1:9/mcp");
    activate_via_real_exchange(
        &coordinator,
        &bound,
        "super-secret-access-token-value",
        Some("refresh-secret-value"),
    )
    .await;

    let mut scanned = 0_usize;
    let mut stack = vec![root.path().to_path_buf()];
    let forbidden: BTreeSet<&str> = ["super-secret-access-token-value", "refresh-secret-value"]
        .into_iter()
        .collect();
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            scanned += 1;
            let text = String::from_utf8_lossy(&bytes);
            for needle in &forbidden {
                assert!(
                    !text.contains(needle),
                    "{} leaked {needle} in {}",
                    path.display(),
                    text
                );
            }
        }
    }
    assert!(scanned > 0, "the scan must actually read persisted files");
}
