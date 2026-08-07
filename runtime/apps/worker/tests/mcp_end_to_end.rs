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
use agent_protocol::{AutoApproval, McpServerSnapshot, SandboxClass, ToolCall};
use agent_runtime_worker::{federated_tool_definitions, GrpcMcpFederationClient, WorkerProcessor};
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
async fn spawn_gateway(private_key_pem: &str) -> String {
    let client = McpFederationClient::from_pkcs8_pem(private_key_pem, Duration::from_secs(5))
        .expect("gateway federation client");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(McpFederationServer::new(McpFederationGrpcService::new(
                client,
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
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let mcp_endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(&private_key_pem).await;

    let tenant_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
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
        .list_tools(tenant_id, run_id, &server, "test-workload-token")
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
            tenant_id,
            run_id,
            &server,
            "mcp:search/web_search",
            &serde_json::json!({ "query": "agent runtime" }),
            &catalog.digest,
            "test-workload-token",
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
            tenant_id,
            run_id,
            &server,
            "mcp:search/web_search",
            &serde_json::json!({ "query": "agent runtime" }),
            &catalog.digest,
            "test-workload-token",
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
