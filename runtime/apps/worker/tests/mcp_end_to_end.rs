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
use agent_protocol::{RunExecutionCommand, RuntimeExecutionPolicySnapshot};
use agent_runtime_worker::FederationIdentity;
use agent_runtime_worker::{
    GrpcMcpFederationClient, McpDiscoveryCompletion, McpDiscoveryCoordinator, McpDiscoveryPolicy,
    McpDiscoveryScheduler, WorkerAssignmentError, WorkerProcessor, WorkerRecoveryAction,
    attach_discovered_federated_tools, discover_federated_tools,
    discover_federated_tools_with_policy, federated_tool_definitions,
};
use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use uuid::Uuid;

/// A minimal MCP server: initialize, tools/list, tools/call.
async fn spawn_mcp_server(tools: Arc<Mutex<Vec<String>>>) -> String {
    spawn_controlled_mcp_server(tools, None).await
}

async fn spawn_modern_mrtr_mcp_server() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
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
                recorded.lock().unwrap().push(request.clone());
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
                            "name": "confirm",
                            "description": "Confirm a search",
                            "inputSchema": {"type": "object"}
                        }],
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }),
                    "tools/call" if request.pointer("/params/inputResponses").is_none() => {
                        serde_json::json!({
                            "resultType": "input_required",
                            "inputRequests": {
                                "confirmation": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "mode": "form",
                                        "message": "Confirm the search",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": {"confirmed": {"type": "boolean"}},
                                            "required": ["confirmed"]
                                        }
                                    }
                                }
                            },
                            "requestState": " opaque/gateway/\u{2603}/state\n"
                        })
                    }
                    "tools/call" => serde_json::json!({
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "confirmed through gateway"}],
                        "isError": false
                    }),
                    method => panic!("unexpected modern MCP method {method}"),
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
    (endpoint, seen)
}

#[derive(Clone)]
struct ListControl {
    seen: Option<Arc<Notify>>,
    release: Option<Arc<Semaphore>>,
}

async fn spawn_controlled_mcp_server(
    tools: Arc<Mutex<Vec<String>>>,
    list_control: Option<ListControl>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tools = Arc::clone(&tools);
            let list_control = list_control.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 32 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let body = if request.contains("\"tools/list\"") {
                    if let Some(control) = list_control {
                        if let Some(seen) = control.seen {
                            seen.notify_one();
                        }
                        if let Some(release) = control.release {
                            release
                                .acquire()
                                .await
                                .expect("list release semaphore must stay open")
                                .forget();
                        }
                    }
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
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}"#.to_owned()
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

fn test_private_key_pem() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        RsaPrivateKey::new(&mut OsRng, 3072)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_federated_tool_is_discovered_registered_gated_and_called() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[71; 32]);
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let mcp_endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;

    let identity = FederationIdentity {
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
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
        required: false,
        tool_effect_overrides: BTreeMap::new(),
        protocol_revision: agent_protocol::McpProtocolRevision::V2025_06_18,
        client_capabilities: BTreeSet::new(),
    };

    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
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
        catalog
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.qualified_name, tool.description, tool.input_schema)),
        &server.tool_effect_overrides,
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
    tools.lock().unwrap().push("delete_everything".to_owned());
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
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[72; 32]);
    let reachable = spawn_mcp_server(Arc::new(Mutex::new(vec!["web_search".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
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
    assert!(
        federated
            .registry
            .authorize(
                "mcp:search/web_search",
                &BTreeSet::from(["tool:mcp:search".to_owned()])
            )
            .is_ok()
    );
    assert!(
        worker
            .tool_registry()
            .authorize(
                "mcp:search/web_search",
                &BTreeSet::from(["tool:mcp:search".to_owned()])
            )
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_slow_mcp_server_does_not_block_other_discovery_and_order_stays_stable() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let first_release = Arc::new(Semaphore::new(0));
    let second_seen = Arc::new(Notify::new());
    let slow = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["slow_tool".to_owned()])),
        Some(ListControl {
            seen: None,
            release: Some(Arc::clone(&first_release)),
        }),
    )
    .await;
    let fast = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["fast_tool".to_owned()])),
        Some(ListControl {
            seen: Some(Arc::clone(&second_seen)),
            release: None,
        }),
    )
    .await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let command = v9_command_with(
        serde_json::json!([
            {
                "server_id": "6f1a9a1a-0000-4000-8000-000000000011",
                "name": "slow",
                "endpoint": slow,
                "credential_envelope_base64": ""
            },
            {
                "server_id": "6f1a9a1a-0000-4000-8000-000000000012",
                "name": "fast",
                "endpoint": fast,
                "credential_envelope_base64": ""
            }
        ]),
        serde_json::json!(["tool:mcp:slow", "tool:mcp:fast"]),
    );
    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let discovery = tokio::spawn(async move {
        let worker = WorkerProcessor::new(
            Uuid::now_v7(),
            vec![agent_protocol::Placement::Cloud],
            4,
            "0.1.0".to_string(),
        )
        .unwrap();
        let mut client = client;
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await
    });

    let fast_started_while_first_was_blocked =
        tokio::time::timeout(Duration::from_secs(1), second_seen.notified())
            .await
            .is_ok();
    first_release.add_permits(1);
    let discovered = discovery.await.unwrap();

    assert!(
        fast_started_while_first_was_blocked,
        "a slow first server must not serialize discovery of the next server"
    );
    assert_eq!(
        discovered
            .definitions
            .iter()
            .map(|definition| definition.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:slow/slow_tool", "mcp:fast/fast_tool"],
        "completion order must not change the command's deterministic catalog order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_discovery_limits_inflight_servers_without_losing_command_order() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    let release = Arc::new(Semaphore::new(0));
    let mut seen = Vec::new();
    let mut servers = Vec::new();
    for index in 0..5 {
        let notified = Arc::new(Notify::new());
        let endpoint = spawn_controlled_mcp_server(
            Arc::new(Mutex::new(vec![format!("tool_{index}")])),
            Some(ListControl {
                seen: Some(Arc::clone(&notified)),
                release: Some(Arc::clone(&release)),
            }),
        )
        .await;
        seen.push(notified);
        servers.push(serde_json::json!({
            "server_id": Uuid::now_v7(),
            "name": format!("server_{index}"),
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }));
    }
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let mut command = v9_command_with(
        serde_json::Value::Array(servers),
        serde_json::json!([
            "tool:mcp:server_0",
            "tool:mcp:server_1",
            "tool:mcp:server_2",
            "tool:mcp:server_3",
            "tool:mcp:server_4"
        ]),
    );
    command.schema_version = 10;
    let mut runtime_policy = RuntimeExecutionPolicySnapshot {
        schema_version: 1,
        ..RuntimeExecutionPolicySnapshot::default()
    };
    runtime_policy.mcp_discovery.max_attempts_per_server = 1;
    runtime_policy.mcp_discovery.initial_retry_backoff_ms = 0;
    runtime_policy.mcp_discovery.max_concurrent_servers = 2;
    runtime_policy.tool_execution.max_concurrent_tools = 1;
    command.runtime_policy = Some(runtime_policy);
    command.validate().unwrap();
    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let discovery = tokio::spawn(async move {
        let worker = WorkerProcessor::new(
            Uuid::now_v7(),
            vec![agent_protocol::Placement::Cloud],
            4,
            "0.1.0".to_string(),
        )
        .unwrap();
        let mut client = client;
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await
    });

    for notified in seen.iter().take(2) {
        tokio::time::timeout(Duration::from_secs(1), notified.notified())
            .await
            .expect("the first two servers should start concurrently");
    }
    let third_started_while_two_were_inflight =
        tokio::time::timeout(Duration::from_millis(250), seen[2].notified())
            .await
            .is_ok();
    release.add_permits(5);
    let discovered = discovery.await.unwrap();

    assert!(
        !third_started_while_two_were_inflight,
        "the command's two-slot policy must keep a third server queued"
    );
    assert_eq!(
        discovered
            .definitions
            .iter()
            .map(|definition| definition.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "mcp:server_0/tool_0",
            "mcp:server_1/tool_1",
            "mcp:server_2/tool_2",
            "mcp:server_3/tool_3",
            "mcp:server_4/tool_4"
        ]
    );
}

/// The per-Run limit above is not enough for a multi-tenant Worker: without a
/// shared admission queue, two four-server Runs can still open eight requests.
/// This drives real MCP sockets and also proves a noisy tenant cannot consume
/// every queued admission ahead of a later tenant.
#[tokio::test(flavor = "multi_thread")]
async fn shared_mcp_discovery_is_globally_bounded_and_tenant_fair() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[78; 32]);
    let release = Arc::new(Semaphore::new(0));
    let mut noisy_seen = Vec::new();
    let mut noisy_servers = Vec::new();
    for index in 0..4 {
        let seen = Arc::new(Notify::new());
        let endpoint = spawn_controlled_mcp_server(
            Arc::new(Mutex::new(vec![format!("noisy_tool_{index}")])),
            Some(ListControl {
                seen: Some(Arc::clone(&seen)),
                release: Some(Arc::clone(&release)),
            }),
        )
        .await;
        noisy_seen.push(seen);
        noisy_servers.push(serde_json::json!({
            "server_id": Uuid::now_v7(),
            "name": format!("noisy_{index}"),
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }));
    }
    let quiet_seen = Arc::new(Notify::new());
    let quiet_endpoint = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["quiet_tool".to_owned()])),
        Some(ListControl {
            seen: Some(Arc::clone(&quiet_seen)),
            release: Some(Arc::clone(&release)),
        }),
    )
    .await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let scheduler = McpDiscoveryScheduler::new(std::num::NonZeroUsize::new(2).unwrap());
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap()
        .with_discovery_scheduler(scheduler.clone());

    let noisy_command = v9_command_with(
        serde_json::Value::Array(noisy_servers),
        serde_json::json!([
            "tool:mcp:noisy_0",
            "tool:mcp:noisy_1",
            "tool:mcp:noisy_2",
            "tool:mcp:noisy_3"
        ]),
    );
    let noisy_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&noisy_command),
    );
    let noisy_registry = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap()
    .tool_registry()
    .clone();
    let mut noisy_client = client.clone();
    let noisy = tokio::spawn(async move {
        discover_federated_tools(
            &noisy_registry,
            &mut noisy_client,
            &noisy_command,
            &noisy_token,
        )
        .await
    });

    for seen in noisy_seen.iter().take(2) {
        tokio::time::timeout(Duration::from_secs(1), seen.notified())
            .await
            .expect("the noisy tenant should fill both shared slots");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(150), noisy_seen[2].notified())
            .await
            .is_err(),
        "a third request must wait behind the Worker's two-slot limit"
    );

    let mut quiet_command = v9_command_with(
        serde_json::json!([{
            "server_id": Uuid::now_v7(),
            "name": "quiet",
            "endpoint": quiet_endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:quiet"]),
    );
    quiet_command.tenant_id = Uuid::now_v7();
    quiet_command.run_id = Uuid::now_v7();
    quiet_command.attempt_id = Uuid::now_v7();
    quiet_command.message_id = Uuid::now_v7();
    quiet_command.lineage.root_run_id = quiet_command.run_id;
    // The fixture's signed Skill snapshot is bound to its original tenant.
    // This test exercises federation admission, so no Skill is needed.
    quiet_command.skill_snapshots.clear();
    quiet_command.validate().unwrap();
    let quiet_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&quiet_command),
    );
    let quiet_registry = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap()
    .tool_registry()
    .clone();
    let mut quiet_client = client;
    let quiet = tokio::spawn(async move {
        discover_federated_tools(
            &quiet_registry,
            &mut quiet_client,
            &quiet_command,
            &quiet_token,
        )
        .await
    });
    // Wait on scheduler observability rather than sleeping and guessing that a
    // loaded CI machine has polled the second Run already.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if scheduler.snapshot().queued_tenants == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both tenant queues should be visible before capacity is released");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), quiet_seen.notified())
            .await
            .is_err(),
        "the aggregate limit must also hold across tenants"
    );

    // Round robin allows one already-queued noisy request, then must admit the
    // quiet tenant before the noisy tenant's fourth request.
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), noisy_seen[2].notified())
        .await
        .expect("the next noisy request should use the first released slot");
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), quiet_seen.notified())
        .await
        .expect("round robin must admit the other tenant before noisy request four");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), noisy_seen[3].notified())
            .await
            .is_err(),
        "one tenant must not drain its whole queue ahead of another tenant"
    );

    release.add_permits(5);
    let (noisy, quiet) = tokio::join!(noisy, quiet);
    assert_eq!(noisy.unwrap().definitions.len(), 4);
    assert_eq!(quiet.unwrap().definitions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_mcp_discovery_releases_shared_capacity_for_the_next_run() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[79; 32]);
    let stalled_seen = Arc::new(Notify::new());
    let stalled_release = Arc::new(Semaphore::new(0));
    let stalled_endpoint = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["stalled_tool".to_owned()])),
        Some(ListControl {
            seen: Some(Arc::clone(&stalled_seen)),
            release: Some(Arc::clone(&stalled_release)),
        }),
    )
    .await;
    let fast_endpoint = spawn_mcp_server(Arc::new(Mutex::new(vec!["fast_tool".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let scheduler = McpDiscoveryScheduler::new(std::num::NonZeroUsize::new(1).unwrap());
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap()
        .with_discovery_scheduler(scheduler);
    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let registry = worker.tool_registry().clone();

    let stalled_command = v9_command_with(
        serde_json::json!([{
            "server_id": Uuid::now_v7(),
            "name": "stalled",
            "endpoint": stalled_endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:stalled"]),
    );
    let stalled_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&stalled_command),
    );
    let mut stalled_client = client.clone();
    let stalled_registry = registry.clone();
    let stalled = tokio::spawn(async move {
        discover_federated_tools_with_policy(
            &stalled_registry,
            &mut stalled_client,
            &stalled_command,
            &stalled_token,
            McpDiscoveryPolicy {
                max_concurrent: std::num::NonZeroUsize::new(1).unwrap(),
                per_server_timeout: Duration::from_secs(5),
                total_timeout: Duration::from_millis(200),
                max_attempts_per_server: 1,
                initial_retry_backoff: Duration::ZERO,
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), stalled_seen.notified())
        .await
        .expect("the first Run should hold the only shared slot");
    let cancelled = tokio::time::timeout(Duration::from_secs(1), stalled)
        .await
        .expect("the Run total budget should cancel stalled discovery")
        .unwrap();
    assert_eq!(cancelled.unavailable.len(), 1);
    assert!(
        cancelled.unavailable[0]
            .1
            .contains("total discovery budget")
    );

    let mut fast_command = v9_command_with(
        serde_json::json!([{
            "server_id": Uuid::now_v7(),
            "name": "fast",
            "endpoint": fast_endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:fast"]),
    );
    fast_command.run_id = Uuid::now_v7();
    fast_command.attempt_id = Uuid::now_v7();
    fast_command.message_id = Uuid::now_v7();
    fast_command.lineage.root_run_id = fast_command.run_id;
    fast_command.validate().unwrap();
    let fast_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&fast_command),
    );
    let mut fast_client = client;
    let fast = tokio::time::timeout(
        Duration::from_secs(1),
        discover_federated_tools(&registry, &mut fast_client, &fast_command, &fast_token),
    )
    .await
    .expect("cancelled discovery must release its shared admission");
    stalled_release.add_permits(1);
    assert_eq!(
        fast.definitions
            .iter()
            .map(|definition| definition.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:fast/fast_tool"]
    );
}

/// The production break this catches is returning an asynchronous discovery
/// result without applying it through the sole Kernel owner. The slow Run holds
/// a real MCP `tools/list`; the fast Run must be attached and started while that
/// request is still blocked.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_run_does_not_block_a_fast_run_in_the_discovery_coordinator() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[80; 32]);
    let slow_seen = Arc::new(Notify::new());
    let slow_release = Arc::new(Semaphore::new(0));
    let slow_endpoint = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["slow_tool".to_owned()])),
        Some(ListControl {
            seen: Some(Arc::clone(&slow_seen)),
            release: Some(Arc::clone(&slow_release)),
        }),
    )
    .await;
    let fast_endpoint = spawn_mcp_server(Arc::new(Mutex::new(vec!["fast_tool".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let skill_signing_key = SigningKey::from_bytes(&[92; 32]);
    let issued_at = chrono::Utc::now();

    let mut slow_command = v9_command_with(
        serde_json::json!([{
            "server_id": Uuid::now_v7(),
            "name": "slow",
            "endpoint": slow_endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:slow"]),
    );
    let slow_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&slow_command),
    );
    slow_command.workload_token = serde_json::from_value(serde_json::json!(slow_token)).unwrap();
    slow_command.issued_at = issued_at;
    slow_command.lease_expires_at = issued_at + chrono::Duration::minutes(5);
    sign_skill_for_federated_tool(&mut slow_command, "mcp:slow/slow_tool", &skill_signing_key);
    slow_command.validate().unwrap();
    let slow_attempt = slow_command.attempt_id;

    let mut fast_command = v9_command_with(
        serde_json::json!([{
            "server_id": Uuid::now_v7(),
            "name": "fast",
            "endpoint": fast_endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:fast"]),
    );
    fast_command.run_id = Uuid::now_v7();
    fast_command.attempt_id = Uuid::now_v7();
    fast_command.message_id = Uuid::now_v7();
    fast_command.lineage.root_run_id = fast_command.run_id;
    let fast_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&fast_command),
    );
    fast_command.workload_token = serde_json::from_value(serde_json::json!(fast_token)).unwrap();
    fast_command.issued_at = issued_at;
    fast_command.lease_expires_at = issued_at + chrono::Duration::minutes(5);
    sign_skill_for_federated_tool(&mut fast_command, "mcp:fast/fast_tool", &skill_signing_key);
    fast_command.validate().unwrap();
    let fast_attempt = fast_command.attempt_id;

    let mut worker = WorkerProcessor::new_with_incarnation(
        slow_command.worker_id,
        slow_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    worker.set_skill_artifact_verifier(agent_runtime_worker::SkillArtifactVerifier::new(
        "local-skill-key",
        skill_signing_key.verifying_key(),
    ));
    worker.accept(slow_command.clone(), issued_at).unwrap();
    worker.accept(fast_command.clone(), issued_at).unwrap();
    let slow_cancellation = worker.cancellation_token(slow_attempt).unwrap();
    let mut coordinator = McpDiscoveryCoordinator::new(8);
    assert!(
        coordinator
            .start(&worker, client.clone(), slow_attempt)
            .unwrap()
    );
    assert!(
        !coordinator
            .start(&worker, client.clone(), slow_attempt)
            .unwrap(),
        "one attempt must have at most one discovery task"
    );
    tokio::time::timeout(Duration::from_secs(1), slow_seen.notified())
        .await
        .expect("the slow Run should be inside a real tools/list request");
    assert!(coordinator.start(&worker, client, fast_attempt).unwrap());

    let completion = coordinator
        .recv_and_apply(&mut worker, Duration::from_secs(1))
        .await
        .unwrap()
        .expect("the fast Run must finish while the slow Run is still blocked");
    match completion {
        McpDiscoveryCompletion::Started {
            attempt_id, event, ..
        } => {
            assert_eq!(attempt_id, fast_attempt);
            assert_eq!(event.event_type, "run.started");
            assert_eq!(
                worker
                    .prepare_model_invocation(fast_attempt)
                    .unwrap()
                    .invocation
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["mcp:fast/fast_tool"],
                "the coordinator must attach the discovered catalog before starting the Run"
            );
        }
        McpDiscoveryCompletion::Cancelled { attempt_id } => {
            panic!("the fast Run was unexpectedly cancelled: {attempt_id}")
        }
        McpDiscoveryCompletion::Restored { attempt_id, .. } => {
            panic!("the new fast Run was unexpectedly treated as restored: {attempt_id}")
        }
    }

    slow_cancellation.cancel();
    assert!(matches!(
        coordinator
            .recv_and_apply(&mut worker, Duration::from_secs(1))
            .await
            .unwrap(),
        Some(McpDiscoveryCompletion::Cancelled { attempt_id }) if attempt_id == slow_attempt
    ));
    slow_release.add_permits(1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_server_hits_the_worker_deadline_without_hiding_fast_tools() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[75; 32]);
    let stalled_release = Arc::new(Semaphore::new(0));
    let stalled = spawn_controlled_mcp_server(
        Arc::new(Mutex::new(vec!["never_returns".to_owned()])),
        Some(ListControl {
            seen: None,
            release: Some(Arc::clone(&stalled_release)),
        }),
    )
    .await;
    let fast = spawn_mcp_server(Arc::new(Mutex::new(vec!["fast_tool".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let command = v9_command_with(
        serde_json::json!([
            {
                "server_id": "6f1a9a1a-0000-4000-8000-000000000021",
                "name": "stalled",
                "endpoint": stalled,
                "credential_envelope_base64": ""
            },
            {
                "server_id": "6f1a9a1a-0000-4000-8000-000000000022",
                "name": "fast",
                "endpoint": fast,
                "credential_envelope_base64": ""
            }
        ]),
        serde_json::json!(["tool:mcp:stalled", "tool:mcp:fast"]),
    );
    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();

    let bounded = tokio::time::timeout(
        Duration::from_secs(4),
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token),
    )
    .await;
    stalled_release.add_permits(1);
    let discovered = bounded.expect("Worker discovery needs its own deadline below the Gateway");

    assert_eq!(
        discovered
            .definitions
            .iter()
            .map(|definition| definition.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:fast/fast_tool"]
    );
    assert_eq!(discovered.unavailable.len(), 1);
    assert_eq!(discovered.unavailable[0].0, "stalled");
    assert!(discovered.unavailable[0].1.contains("deadline"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_total_discovery_budget_keeps_completed_tools_and_cancels_the_rest_in_order() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[76; 32]);
    let stalled_release = Arc::new(Semaphore::new(0));
    let fast = spawn_mcp_server(Arc::new(Mutex::new(vec!["fast_tool".to_owned()]))).await;
    let mut servers = vec![serde_json::json!({
        "server_id": Uuid::now_v7(),
        "name": "fast",
        "endpoint": fast,
        "credential_envelope_base64": ""
    })];
    for index in 1..5 {
        let endpoint = spawn_controlled_mcp_server(
            Arc::new(Mutex::new(vec![format!("never_{index}")])),
            Some(ListControl {
                seen: None,
                release: Some(Arc::clone(&stalled_release)),
            }),
        )
        .await;
        servers.push(serde_json::json!({
            "server_id": Uuid::now_v7(),
            "name": format!("stalled_{index}"),
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }));
    }
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let command = v9_command_with(
        serde_json::Value::Array(servers),
        serde_json::json!([
            "tool:mcp:fast",
            "tool:mcp:stalled_1",
            "tool:mcp:stalled_2",
            "tool:mcp:stalled_3",
            "tool:mcp:stalled_4"
        ]),
    );
    let token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let policy = McpDiscoveryPolicy {
        max_concurrent: std::num::NonZeroUsize::new(1).unwrap(),
        per_server_timeout: Duration::from_secs(2),
        total_timeout: Duration::from_millis(250),
        max_attempts_per_server: 1,
        initial_retry_backoff: Duration::ZERO,
    };

    let bounded = tokio::time::timeout(
        Duration::from_secs(1),
        discover_federated_tools_with_policy(
            worker.tool_registry(),
            &mut client,
            &command,
            &token,
            policy,
        ),
    )
    .await;
    stalled_release.add_permits(4);
    let discovered = bounded.expect("the total discovery budget must cancel the whole batch");

    assert_eq!(
        discovered
            .definitions
            .iter()
            .map(|definition| definition.descriptor.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:fast/fast_tool"]
    );
    assert_eq!(
        discovered
            .unavailable
            .iter()
            .map(|(server, _)| server.as_str())
            .collect::<Vec<_>>(),
        vec!["stalled_1", "stalled_2", "stalled_3", "stalled_4"]
    );
    assert!(
        discovered
            .unavailable
            .iter()
            .all(|(_, reason)| reason.contains("total discovery budget"))
    );
    assert_eq!(
        discovered
            .statuses
            .iter()
            .map(|status| status.attempts)
            .collect::<Vec<_>>(),
        vec![1, 0, 0, 0, 0],
        "servers cancelled before a discovery result must not report a completed attempt"
    );
}

/// Two Runs against the same server, frozen at different catalogs.
///
/// This is why the registry is per-Run. A shared, name-keyed registry could hold
/// only one of these digests, and the second Run would either fail to register or
/// silently inherit the first Run's freeze.
#[tokio::test(flavor = "multi_thread")]
async fn two_runs_hold_different_freezes_of_the_same_server() {
    let private_key_pem = test_private_key_pem();
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let signing_key = SigningKey::from_bytes(&[73; 32]);
    let endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
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

/// The production break this catches is declaring recovery ready after network
/// discovery but before the exact checkpointed MCP bindings are reattached and
/// verified. A restored Run may only resume model work with the same Tool set.
#[tokio::test(flavor = "multi_thread")]
async fn coordinator_verifies_the_frozen_catalog_before_a_restored_run_resumes() {
    let private_key_pem = test_private_key_pem();
    let gateway_signing_key = SigningKey::from_bytes(&[78; 32]);
    let skill_signing_key = SigningKey::from_bytes(&[93; 32]);
    let endpoint = spawn_mcp_server(Arc::new(Mutex::new(vec!["web_search".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &gateway_signing_key).await;
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let issued_at = chrono::Utc::now();
    let mut command = v9_command_with(
        serde_json::json!([{
            "server_id": "6f1a9a1a-0000-4000-8000-000000000032",
            "name": "search",
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:search"]),
    );
    command.issued_at = issued_at;
    command.lease_expires_at = issued_at + chrono::Duration::minutes(5);
    command.workload_token = serde_json::from_value(serde_json::json!(signed_identity(
        &gateway_signing_key,
        &FederationIdentity::from_command(&command),
    )))
    .unwrap();
    sign_skill_for_federated_tool(&mut command, "mcp:search/web_search", &skill_signing_key);
    command.validate().unwrap();

    let mut original = WorkerProcessor::new_with_incarnation(
        command.worker_id,
        command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    original.set_skill_artifact_verifier(agent_runtime_worker::SkillArtifactVerifier::new(
        "local-skill-key",
        skill_signing_key.verifying_key(),
    ));
    original.accept(command.clone(), issued_at).unwrap();
    let mut original_coordinator = McpDiscoveryCoordinator::new(4);
    assert!(
        original_coordinator
            .start(&original, client.clone(), command.attempt_id)
            .unwrap()
    );
    assert!(matches!(
        original_coordinator
            .recv_and_apply(&mut original, Duration::from_secs(1))
            .await
            .unwrap(),
        Some(McpDiscoveryCompletion::Started { attempt_id, .. })
            if attempt_id == command.attempt_id
    ));
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = Uuid::now_v7();
    replacement_command.worker_incarnation_id = Uuid::now_v7();
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = chrono::Utc::now();
    replacement_command.lease_expires_at =
        replacement_command.issued_at + chrono::Duration::minutes(5);
    replacement_command.workload_token =
        serde_json::from_value(serde_json::json!(signed_identity(
            &gateway_signing_key,
            &FederationIdentity::from_command(&replacement_command),
        )))
        .unwrap();
    replacement_command.validate().unwrap();
    let replacement_attempt = replacement_command.attempt_id;
    let mut replacement = WorkerProcessor::new_with_incarnation(
        replacement_command.worker_id,
        replacement_command.worker_incarnation_id,
        vec![agent_protocol::Placement::Cloud],
        1,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement.set_skill_artifact_verifier(agent_runtime_worker::SkillArtifactVerifier::new(
        "local-skill-key",
        skill_signing_key.verifying_key(),
    ));
    replacement
        .restore(replacement_command, checkpoint, chrono::Utc::now())
        .unwrap();
    assert_eq!(
        replacement
            .verify_restored_federated_tools(replacement_attempt)
            .expect_err("a restored Run is not resumable before exact MCP reattachment"),
        WorkerAssignmentError::CheckpointToolCatalogMismatch
    );

    let mut replacement_coordinator = McpDiscoveryCoordinator::new(4);
    assert!(
        replacement_coordinator
            .start(&replacement, client, replacement_attempt)
            .unwrap()
    );
    assert!(matches!(
        replacement_coordinator
            .recv_and_apply(&mut replacement, Duration::from_secs(1))
            .await
            .unwrap(),
        Some(McpDiscoveryCompletion::Restored { attempt_id, .. })
            if attempt_id == replacement_attempt
    ));
    assert_eq!(
        replacement.recovery_action(replacement_attempt).unwrap(),
        WorkerRecoveryAction::InvokeModel
    );
    assert_eq!(
        replacement
            .prepare_model_invocation(replacement_attempt)
            .unwrap()
            .invocation
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp:search/web_search"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_recovery_rejects_the_same_catalog_under_a_different_discovery_policy() {
    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[77; 32]);
    let endpoint = spawn_mcp_server(Arc::new(Mutex::new(vec!["web_search".to_owned()]))).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let original_worker_id = Uuid::now_v7();
    let mut command = v9_command_with(
        serde_json::json!([{
            "server_id": "6f1a9a1a-0000-4000-8000-000000000031",
            "name": "search",
            "endpoint": endpoint,
            "credential_envelope_base64": ""
        }]),
        serde_json::json!(["tool:mcp:search"]),
    );
    command.skill_snapshots.clear();
    command.worker_id = original_worker_id;
    command.worker_incarnation_id = original_worker_id;
    command.issued_at = chrono::Utc::now();
    command.lease_expires_at = command.issued_at + chrono::Duration::minutes(5);
    let original_token = signed_identity(&signing_key, &FederationIdentity::from_command(&command));
    command.workload_token = serde_json::from_value(serde_json::json!(original_token)).unwrap();
    command.validate().unwrap();
    let original_policy = McpDiscoveryPolicy {
        max_concurrent: std::num::NonZeroUsize::new(4).unwrap(),
        per_server_timeout: Duration::from_secs(3),
        total_timeout: Duration::from_secs(10),
        max_attempts_per_server: 1,
        initial_retry_backoff: Duration::ZERO,
    };
    let mut original = WorkerProcessor::new(
        original_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    original
        .accept(command.clone(), chrono::Utc::now())
        .unwrap();
    let discovered = discover_federated_tools_with_policy(
        original.tool_registry(),
        &mut client,
        &command,
        command.workload_token.as_str(),
        original_policy,
    )
    .await;
    attach_discovered_federated_tools(
        &mut original,
        client.clone(),
        &command,
        command.attempt_id,
        discovered,
    )
    .unwrap();
    original.start(command.attempt_id).unwrap();
    let checkpoint = original.checkpoint(command.attempt_id).unwrap();

    let replacement_worker_id = Uuid::now_v7();
    let mut replacement_command = command;
    replacement_command.message_id = Uuid::now_v7();
    replacement_command.attempt_id = Uuid::now_v7();
    replacement_command.worker_id = replacement_worker_id;
    replacement_command.worker_incarnation_id = replacement_worker_id;
    replacement_command.owner_epoch += 1;
    replacement_command.fencing_token = Uuid::now_v7();
    replacement_command.issued_at = chrono::Utc::now();
    replacement_command.lease_expires_at =
        replacement_command.issued_at + chrono::Duration::minutes(5);
    let replacement_token = signed_identity(
        &signing_key,
        &FederationIdentity::from_command(&replacement_command),
    );
    replacement_command.workload_token =
        serde_json::from_value(serde_json::json!(replacement_token)).unwrap();
    let mut replacement = WorkerProcessor::new(
        replacement_worker_id,
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    replacement
        .restore(
            replacement_command.clone(),
            checkpoint,
            replacement_command.issued_at + chrono::Duration::seconds(1),
        )
        .unwrap();
    let changed_policy = McpDiscoveryPolicy {
        max_concurrent: std::num::NonZeroUsize::new(2).unwrap(),
        ..original_policy
    };
    let rediscovered = discover_federated_tools_with_policy(
        replacement.tool_registry(),
        &mut client,
        &replacement_command,
        replacement_command.workload_token.as_str(),
        changed_policy,
    )
    .await;

    assert_eq!(
        attach_discovered_federated_tools(
            &mut replacement,
            client,
            &replacement_command,
            replacement_command.attempt_id,
            rediscovered,
        )
        .expect_err("a recovered Run must keep the discovery policy it checkpointed"),
        WorkerAssignmentError::CheckpointToolCatalogMismatch
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
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: identity.run_id,
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: identity.attempt_id,
        worker_id: identity.worker_id,
        worker_incarnation_id: identity.worker_incarnation_id,
        model_policy_id: Uuid::now_v7(),
        model_policy_digest: String::new(),
        authorized_mcp_servers: Default::default(),
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

fn sign_skill_for_federated_tool(
    command: &mut RunExecutionCommand,
    tool_name: &str,
    signing_key: &SigningKey,
) {
    let snapshot = command
        .skill_snapshots
        .first_mut()
        .expect("the command fixture carries one Skill snapshot");
    snapshot.tool_names = vec![tool_name.to_owned()];
    let digest = snapshot.expected_artifact_digest(command.tenant_id);
    snapshot.artifact_digest = digest.clone();
    snapshot.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        signing_key
            .sign(format!("agent-runtime-skill-v1.{digest}").as_bytes())
            .to_bytes(),
    );
}

/// The main chain: a federated tool goes through the same executor path a native
/// Tool does.
///
/// Everything before this drove the pieces by hand. This drives
/// `prepare_tool_launch` -> `ToolExecutor::execute`, which is what the Worker's
/// own loop calls, so approval, the started and result events and the checkpoint
/// all run unchanged rather than being reimplemented for federation.
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_executor_path_runs_a_federated_tool() {
    use agent_protocol::{SandboxClass as Sandbox, ToolExecutionRequest};
    use agent_runtime_worker::FederatedToolExecutor;
    use agent_tool_runtime::{ToolExecutionContext, ToolExecutor};

    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[74; 32]);
    let tools = Arc::new(Mutex::new(vec!["web_search".to_owned()]));
    let endpoint = spawn_mcp_server(Arc::clone(&tools)).await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let mut client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
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
    let identity = FederationIdentity::from_command(&command);
    let token = signed_identity(&signing_key, &identity);
    let worker = WorkerProcessor::new(
        Uuid::now_v7(),
        vec![agent_protocol::Placement::Cloud],
        4,
        "0.1.0".to_string(),
    )
    .unwrap();
    let federated =
        discover_federated_tools(worker.tool_registry(), &mut client, &command, &token).await;
    let digest = federated.frozen_digests["search"].clone();

    let executor = FederatedToolExecutor::new(
        client,
        command.mcp_servers[0].clone(),
        identity,
        digest.clone(),
        token,
    );
    // The digest the descriptor carries and the digest the executor presents
    // have to be the same value, or registration would reject the pair.
    assert_eq!(executor.implementation_digest(), digest);

    let request = ToolExecutionRequest {
        call: ToolCall {
            id: "call-1".into(),
            name: "mcp:search/web_search".into(),
            arguments: serde_json::json!({ "query": "agent runtime" }),
        },
        effect: agent_protocol::ToolEffect::Unknown,
        sandbox: Sandbox::Federated,
        binding_digest: "b".repeat(64),
    };
    let context = ToolExecutionContext {
        tenant_id: identity.tenant_id,
        application_id: identity.application_id,
        workload_identity_id: identity.workload_identity_id,
        run_id: identity.run_id,
        session_id: identity.session_id,
        workspace_id: identity.workspace_id,
        agent_version_id: identity.agent_version_id,
        attempt_id: identity.attempt_id,
        workspace_root: std::path::PathBuf::from("/tmp"),
        timeout: Duration::from_secs(10),
        cancellation: tokio_util::sync::CancellationToken::new(),
        requested_at: chrono::Utc::now(),
    };

    let result = executor
        .execute(request.clone(), context.clone())
        .await
        .expect("a federated tool should execute through the normal path");
    assert!(!result.is_error);
    assert_eq!(result.exit_code, 0);
    assert!(result.content.to_string().contains("three results"));

    // A request routed to the wrong executor must be refused rather than
    // silently called anyway.
    let mut wrong = request.clone();
    wrong.sandbox = Sandbox::TrustedNative;
    assert!(matches!(
        executor.execute(wrong, context.clone()).await,
        Err(agent_tool_runtime::ToolExecutionError::WrongSandbox)
    ));

    // And an executor built for one Run must refuse another Run's context.
    let mut foreign = context.clone();
    foreign.run_id = Uuid::now_v7();
    assert!(matches!(
        executor.execute(request.clone(), foreign).await,
        Err(agent_tool_runtime::ToolExecutionError::InvalidContext(_))
    ));

    // The freeze still holds through the executor, and the failure is an
    // execution failure rather than something the Worker would retry.
    tools.lock().unwrap().push("delete_everything".to_owned());
    let refused = executor
        .execute(request, context)
        .await
        .expect_err("a moved catalog must refuse through the executor too");
    assert!(
        refused.to_string().contains("catalog changed"),
        "expected the catalog refusal, got {refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_worker_gateway_path_preserves_a_modern_mcp_input_round() {
    use agent_protocol::{
        McpClientCapability, McpInputAction, McpInputContinuation, McpInputResponse,
        McpProtocolRevision, SandboxClass as Sandbox, ToolExecutionRequest,
    };
    use agent_runtime_worker::FederatedToolExecutor;
    use agent_tool_runtime::{ToolExecutionContext, ToolExecutionError, ToolExecutor};

    let private_key_pem = test_private_key_pem();
    let signing_key = SigningKey::from_bytes(&[75; 32]);
    let (endpoint, seen) = spawn_modern_mrtr_mcp_server().await;
    let gateway_endpoint = spawn_gateway(private_key_pem, &signing_key).await;
    let client = GrpcMcpFederationClient::connect(gateway_endpoint)
        .await
        .unwrap();
    let identity = FederationIdentity {
        tenant_id: Uuid::now_v7(),
        application_id: Uuid::nil(),
        workload_identity_id: Uuid::nil(),
        run_id: Uuid::now_v7(),
        session_id: Uuid::nil(),
        workspace_id: Uuid::nil(),
        agent_version_id: Uuid::nil(),
        attempt_id: Uuid::now_v7(),
        worker_id: Uuid::now_v7(),
        worker_incarnation_id: Uuid::now_v7(),
    };
    let token = signed_identity(&signing_key, &identity);
    let server = McpServerSnapshot {
        server_id: Uuid::now_v7(),
        name: "modern".into(),
        endpoint,
        credential_envelope_base64: String::new(),
        required: true,
        tool_effect_overrides: BTreeMap::new(),
        protocol_revision: McpProtocolRevision::V2026_07_28,
        client_capabilities: BTreeSet::from([McpClientCapability::Elicitation]),
    };
    let catalog = client
        .list_tools(&identity, &server, &token)
        .await
        .expect("modern discovery should traverse the gateway");
    let executor = FederatedToolExecutor::new(client, server, identity, catalog.digest, token);
    let request = ToolExecutionRequest {
        call: ToolCall {
            id: "call-modern-1".into(),
            name: "mcp:modern/confirm".into(),
            arguments: serde_json::json!({"query": "agent runtime"}),
        },
        effect: agent_protocol::ToolEffect::Unknown,
        sandbox: Sandbox::Federated,
        binding_digest: "c".repeat(64),
    };
    let context = ToolExecutionContext {
        tenant_id: identity.tenant_id,
        application_id: identity.application_id,
        workload_identity_id: identity.workload_identity_id,
        run_id: identity.run_id,
        session_id: identity.session_id,
        workspace_id: identity.workspace_id,
        agent_version_id: identity.agent_version_id,
        attempt_id: identity.attempt_id,
        workspace_root: std::path::PathBuf::from("/tmp"),
        timeout: Duration::from_secs(10),
        cancellation: tokio_util::sync::CancellationToken::new(),
        requested_at: chrono::Utc::now(),
    };

    let first = executor
        .execute(request.clone(), context.clone())
        .await
        .expect_err("the first round must suspend for user input");
    let ToolExecutionError::McpInputRequired {
        round,
        request_state,
        requests,
    } = first
    else {
        panic!("gateway must return typed MCP input, got {first:?}");
    };
    assert_eq!(round, 1);
    assert_eq!(request_state, " opaque/gateway/\u{2603}/state\n");
    assert_eq!(requests.keys().collect::<Vec<_>>(), vec!["confirmation"]);

    let result = executor
        .resume_with_mcp_input(
            request,
            context,
            McpInputContinuation {
                round: 2,
                request_state,
                responses: BTreeMap::from([(
                    "confirmation".into(),
                    McpInputResponse {
                        action: McpInputAction::Accept,
                        content: Some(serde_json::json!({"confirmed": true})),
                        meta: None,
                    },
                )]),
            },
            agent_tool_runtime::ToolProgressReporter::disabled(),
        )
        .await
        .expect("the persisted continuation must complete through gRPC");
    assert!(!result.is_error);
    assert!(
        result
            .content
            .to_string()
            .contains("confirmed through gateway")
    );

    let calls = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == "tools/call")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        2,
        "the Tool must run exactly one round per input state"
    );
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(
        calls[1].pointer("/params/requestState"),
        Some(&serde_json::json!(" opaque/gateway/\u{2603}/state\n"))
    );
    assert_eq!(
        calls[1].pointer("/params/inputResponses/confirmation/content/confirmed"),
        Some(&serde_json::json!(true))
    );
}
