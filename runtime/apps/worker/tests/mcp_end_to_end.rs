//! The whole federated chain, with a real socket at every hop (ADR-0040).
//!
//! MCP server (HTTP, loopback) <- gateway (federation client) <- gateway gRPC
//! server <- Worker gRPC client <- kernel Tool registry.
//!
//! Each slice so far tested its own hop. This is the one that fails if any joint
//! between them is wrong, which is the only thing that can tell "five pieces
//! exist" apart from "the thing works".

use agent_kernel::ToolPlan;
use agent_model_gateway::mcp::McpFederationClient;
use agent_model_gateway::mcp_grpc::McpFederationGrpcService;
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederationServer;
use agent_protocol::RunExecutionCommand;
use agent_protocol::{AutoApproval, McpServerSnapshot, SandboxClass, ToolCall};
use agent_runtime_worker::FederationIdentity;
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use agent_runtime_worker::{
    discover_federated_tools, federated_tool_definitions, GrpcMcpFederationClient,
    WorkerProcessor,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use uuid::Uuid;

/// A minimal MCP server: initialize, tools/list, tools/call.
async fn spawn_mcp_server(tools: Arc<Mutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tools = Arc::clone(&tools);
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 32 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let body = if request.contains("\"tools/list\"") {
                    let listed = tools
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| {
                            format!(
                                r#"{{"name":"{name}","description":"Search the web","inputSchema":{{"type":"object","properties":{{"query":{{"type":"string"}}}}}}}}"#
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{listed}]}}}}"#)
                } else if request.contains("\"tools/call\"") {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"three results"}],"isError":false}}"#.to_owned()
                } else {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    format!("http://{address}/rpc")
}

/// The gateway, serving the federation RPCs over plaintext gRPC on loopback.
///
/// No mTLS here: this test is about the federation path, and the transport
/// security of the gateway has its own coverage. Saying so rather than leaving
/// it to be inferred.
async fn spawn_gateway(private_key_pem: &str, signing_key: &SigningKey) -> String {
    let client = McpFederationClient::from_pkcs8_pem(private_key_pem, Duration::from_secs(5), true)
        .expect("gateway federation client");
    let verifier = WorkloadTokenVerifier::new(signing_key.verifying_key());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(McpFederationServer::new(McpFederationGrpcService::new(
                client, verifier,
            )))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .ok();
    });
    format!("http://{address}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_federated_tool_is_discovered_registered_gated_and_called() {
    let private_key_pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    let signing_key = SigningKey::from_bytes(&[71; 32]);
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let mcp_endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(&private_key_pem, &signing_key).await;

    let identity = FederationIdentity {
        tenant_id: Uuid::now_v7(),
        run_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
    };
    let token = signed_identity(&signing_key, &identity);
    // No credential: an open server, which is the case that needs no key
    // material in a test and exercises the same path.
    let server = McpServerSnapshot {
        server_id: Uuid::now_v7(),
        name: "search".into(),
        endpoint: mcp_endpoint,
        credential_envelope_base64: String::new(),
    };

    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .expect("worker should reach the gateway");

    // 1. Discovery, through the gateway, from the real MCP server.
    let catalog = client
        .list_tools(&identity, &server, &token)
        .await
        .expect("discovery should succeed");
    assert_eq!(catalog.tools.len(), 1);
    assert_eq!(catalog.tools[0].qualified_name, "mcp:search/web_search");
    assert_eq!(catalog.digest.len(), 64);

    // 2. Registration into the kernel's Tool registry.
    let mut worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let definitions = federated_tool_definitions(
        &server.name,
        &catalog.digest,
        catalog.tools.iter().cloned().map(|tool| {
            (tool.qualified_name, tool.description, tool.input_schema)
        }),
    )
    .expect("discovered tools should be registerable");
    for definition in definitions {
        assert_eq!(definition.descriptor.sandbox, SandboxClass::Federated);
        worker.register_tool(definition).unwrap();
    }

    // 3. The kernel gates it -- with the tenant's own exemption configured, and
    //    an argument shaped to trigger it.
    let plan = worker
        .tool_registry()
        .plan(
            ToolCall {
                id: "call-1".into(),
                name: "mcp:search/web_search".into(),
                arguments: serde_json::json!({ "command": "ls -la" }),
            },
            &BTreeSet::from(["tool:mcp:search".to_owned()]),
            &BTreeMap::from([(
                "mcp:search/web_search".to_owned(),
                AutoApproval::ProvablyReadOnlyShellCommand,
            )]),
        )
        .unwrap();
    assert!(
        matches!(plan, ToolPlan::ApprovalRequired(_)),
        "a federated tool asks even with an exemption configured, got {plan:?}"
    );

    // 4. Approved, the call reaches the MCP server and the result comes back.
    let (content, is_error) = client
        .call_tool(
            &identity,
            &server,
            "mcp:search/web_search",
            &serde_json::json!({ "query": "agent runtime" }),
            &catalog.digest,
            &token,
        )
        .await
        .expect("an approved call should reach the server");
    assert!(!is_error);
    assert!(
        content.to_string().contains("three results"),
        "the server's own answer should come back, got {content}"
    );

    // 5. The freeze holds across the whole chain, not just inside the gateway.
    tools
        .lock()
        .unwrap()
        .push("delete_everything".to_owned());
    let refused = client
        .call_tool(
            &identity,
            &server,
            "mcp:search/web_search",
            &serde_json::json!({ "query": "agent runtime" }),
            &catalog.digest,
            &token,
        )
        .await
        .expect_err("a changed catalog must be refused end to end");
    assert!(
        !refused.is_retryable(),
        "a refusal is not a transport failure and must not be retried: {refused:?}"
    );
    assert!(
        refused.to_string().contains("catalog changed"),
        "expected the catalog refusal, got {refused}"
    );
}

const EXECUTION_V6_EXAMPLE: &str =
    include_str!("../../../../contracts/events/run-execution-requested.v6.example.json");

fn v9_command_with(servers: serde_json::Value, scopes: serde_json::Value) -> RunExecutionCommand {
    let mut value: serde_json::Value = serde_json::from_str(EXECUTION_V6_EXAMPLE).unwrap();
    value["schema_version"] = serde_json::json!(9);
    value["delegated_scopes"] = scopes;
    value["mcp_servers"] = servers;
    let command: RunExecutionCommand = serde_json::from_value(value).unwrap();
    command.validate().expect("the command must be valid");
    command
}

/// The automatic path: everything a Run needs comes from the command, and
/// discovery happens once at the start.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_discovers_and_freezes_its_servers_from_the_command() {
    let private_key_pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    let signing_key = SigningKey::from_bytes(&[72; 32]);
    let reachable = spawn_mcp_server(Arc::new(Mutex::new(vec!["web_search".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(&private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();

    let command = v9_command_with(
        serde_json::json!([
            {
                "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
                "name": "search",
                "endpoint": reachable,
                "credential_envelope_base64": ""
            },
            {
                // Nothing listening. One unreachable third-party server must not
                // fail a Run that may never use it, and must not vanish either.
                "server_id": "6f1a9a1a-0000-4000-8000-000000000002",
                "name": "down",
                "endpoint": "http://127.0.0.1:1/rpc",
                "credential_envelope_base64": ""
            }
        ]),
        serde_json::json!(["tool:mcp:search", "tool:mcp:down"]),
    );

    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();

    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let federated =
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await;

    assert_eq!(federated.definitions.len(), 1);
    assert_eq!(
        federated.frozen_digests.keys().collect::<Vec<_>>(),
        vec!["search"],
        "only the server that answered gets a frozen digest"
    );
    assert_eq!(
        federated.unavailable.len(),
        1,
        "the unreachable server must be reported, not dropped: {:?}",
        federated.unavailable
    );
    assert_eq!(federated.unavailable[0].0, "down");

    // The Run's registry can plan the discovered tool; the Worker's own cannot,
    // which is what per-Run scoping means.
    assert!(federated
        .registry
        .authorize(
            "mcp:search/web_search",
            &BTreeSet::from(["tool:mcp:search".to_owned()])
        )
        .is_ok());
    assert!(worker
        .tool_registry()
        .authorize(
            "mcp:search/web_search",
            &BTreeSet::from(["tool:mcp:search".to_owned()])
        )
        .is_err());
}

/// Two Runs against the same server, frozen at different catalogs.
///
/// This is why the registry is per-Run. A shared, name-keyed registry could hold
/// only one of these digests, and the second Run would either fail to register or
/// silently inherit the first Run's freeze.
#[tokio::test(flavor = "multi_thread")]
async fn two_runs_hold_different_freezes_of_the_same_server() {
    let private_key_pem = RsaPrivateKey::new(&mut OsRng, 3072)
        .unwrap()
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(&private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let command = v9_command_with(
        serde_json::json!([{
            "server_id": "6f1a9a1a-0000-4000-8000-000000000001",
            "name": "search",
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:search"]),
    );

    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let first =
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await;
    tools.lock().unwrap().push("summarise".to_owned());
    let second =
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await;

    assert_eq!(first.definitions.len(), 1);
    assert_eq!(second.definitions.len(), 2);
    assert_ne!(
        first.frozen_digests["search"], second.frozen_digests["search"],
        "each Run freezes what it discovered, not what the other did"
    );
}

/// A workload token bound to the identity the calls will present.
///
/// The federation RPCs verify this now, so a test that skipped it would only be
/// proving the chain works for a caller nobody authenticated.
fn signed_identity(signing_key: &SigningKey, identity: &FederationIdentity) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let claims = WorkloadIdentityClaims {
        schema_version: 2,
        tenant_id: identity.tenant_id,
        run_id: identity.run_id,
        attempt_id: identity.attempt_id,
        worker_id: identity.worker_id,
        worker_incarnation_id: identity.worker_incarnation_id,
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        audiences: std::collections::BTreeSet::from(["model-gateway".to_owned()]),
        scopes: std::collections::BTreeSet::from(["mcp.federate".to_owned()]),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + 60_000,
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("v2.{payload}");
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing_key.sign(signing_input.as_bytes()).to_bytes());
    format!("{signing_input}.{signature}")
}
