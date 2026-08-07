//! Worker-side client for federated MCP calls (ADR-0040).
//!
//! The Worker never opens a sealed credential and never reaches an MCP server
//! directly. It hands the sealed envelope to the gateway, which opens it and
//! makes the call. What travels back is a bounded result.

use agent_model_gateway_protocol::v1::mcp_federation_client::McpFederationClient;
use agent_model_gateway_protocol::v1::{
    McpCallToolRequest, McpListToolsRequest, McpServerRef as WireServerRef,
};
use agent_protocol::McpServerSnapshot;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone)]
pub struct GrpcMcpFederationClient {
    inner: McpFederationClient<Channel>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpGatewayClientError {
    #[error("mcp gateway transport failed: {0}")]
    Transport(String),
    #[error("mcp gateway RPC failed with {code}: {message}")]
    Rpc { code: Code, message: String },
    #[error("mcp gateway returned an invalid response: {0}")]
    InvalidResponse(String),
}

impl McpGatewayClientError {
    /// Whether a caller may retry.
    ///
    /// A refused call is not a failed one. `FailedPrecondition` means the
    /// catalog moved or the tool was never in it, and retrying that is retrying
    /// a security decision until it changes its mind.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            McpGatewayClientError::Transport(_)
                | McpGatewayClientError::Rpc {
                    code: Code::Unavailable | Code::DeadlineExceeded,
                    ..
                }
        )
    }
}

/// One federated tool as the gateway described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredTool {
    pub qualified_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredCatalog {
    pub tools: Vec<DiscoveredTool>,
    pub digest: String,
}

impl GrpcMcpFederationClient {
    pub async fn connect(endpoint: String) -> Result<Self, McpGatewayClientError> {
        let inner = McpFederationClient::connect(endpoint)
            .await
            .map_err(|error| McpGatewayClientError::Transport(error.to_string()))?;
        Ok(Self { inner })
    }

    pub async fn connect_with_mtls(
        endpoint: String,
        materials: agent_grpc_security::ClientMtlsMaterials,
    ) -> Result<Self, McpGatewayClientError> {
        let endpoint = Endpoint::from_shared(endpoint)
            .map_err(|error| McpGatewayClientError::Transport(error.to_string()))?
            .tls_config(materials.into_tonic())
            .map_err(|error| McpGatewayClientError::Transport(error.to_string()))?;
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| McpGatewayClientError::Transport(error.to_string()))?;
        Ok(Self {
            inner: McpFederationClient::new(channel),
        })
    }

    pub async fn list_tools(
        &mut self,
        tenant_id: Uuid,
        run_id: Uuid,
        server: &McpServerSnapshot,
        workload_token: &str,
    ) -> Result<DiscoveredCatalog, McpGatewayClientError> {
        let mut request = tonic::Request::new(McpListToolsRequest {
            schema_version: SCHEMA_VERSION,
            tenant_id: tenant_id.to_string(),
            run_id: run_id.to_string(),
            server: Some(wire_server(server)?),
        });
        authorize(&mut request, workload_token)?;
        let response = self.inner.list_tools(request).await.map_err(rpc_error)?;
        let response = response.into_inner();
        let mut tools = Vec::with_capacity(response.tools.len());
        for tool in response.tools {
            let schema = std::str::from_utf8(&tool.input_schema_json)
                .map_err(|_| {
                    McpGatewayClientError::InvalidResponse("input schema is not utf-8".into())
                })
                .and_then(|text| {
                    serde_json::from_str::<serde_json::Value>(text).map_err(|error| {
                        McpGatewayClientError::InvalidResponse(error.to_string())
                    })
                })?;
            tools.push(DiscoveredTool {
                qualified_name: tool.qualified_name,
                description: tool.description,
                input_schema: schema,
            });
        }
        // A catalog with no digest cannot be frozen, and a Tool registered
        // against an empty implementation digest would be refused later with a
        // less useful message than this one.
        if response.catalog_digest.len() != 64 {
            return Err(McpGatewayClientError::InvalidResponse(
                "catalog digest is not a sha256".into(),
            ));
        }
        Ok(DiscoveredCatalog {
            tools,
            digest: response.catalog_digest,
        })
    }

    pub async fn call_tool(
        &mut self,
        tenant_id: Uuid,
        run_id: Uuid,
        server: &McpServerSnapshot,
        qualified_name: &str,
        arguments: &serde_json::Value,
        frozen_catalog_digest: &str,
        workload_token: &str,
    ) -> Result<(serde_json::Value, bool), McpGatewayClientError> {
        let mut request = tonic::Request::new(McpCallToolRequest {
            schema_version: SCHEMA_VERSION,
            tenant_id: tenant_id.to_string(),
            run_id: run_id.to_string(),
            server: Some(wire_server(server)?),
            qualified_name: qualified_name.to_owned(),
            arguments_json: arguments.to_string().into_bytes().into(),
            frozen_catalog_digest: frozen_catalog_digest.to_owned(),
        });
        authorize(&mut request, workload_token)?;
        let response = self.inner.call_tool(request).await.map_err(rpc_error)?;
        let response = response.into_inner();
        let content = std::str::from_utf8(&response.content_json)
            .map_err(|_| McpGatewayClientError::InvalidResponse("content is not utf-8".into()))
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(text)
                    .map_err(|error| McpGatewayClientError::InvalidResponse(error.to_string()))
            })?;
        Ok((content, response.is_error))
    }
}

fn wire_server(server: &McpServerSnapshot) -> Result<WireServerRef, McpGatewayClientError> {
    use base64::Engine;
    // The Worker holds the envelope base64-encoded and never decodes it as a
    // credential -- this is a transport re-encoding, not an opening.
    let envelope = if server.credential_envelope_base64.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(&server.credential_envelope_base64)
            .map_err(|_| {
                McpGatewayClientError::Transport("credential envelope is not base64".into())
            })?
    };
    Ok(WireServerRef {
        server_id: server.server_id.to_string(),
        name: server.name.clone(),
        endpoint: server.endpoint.clone(),
        credential_envelope_json: envelope.into(),
    })
}

fn authorize<T>(
    request: &mut tonic::Request<T>,
    workload_token: &str,
) -> Result<(), McpGatewayClientError> {
    let authorization = MetadataValue::try_from(format!("Bearer {workload_token}"))
        .map_err(|_| McpGatewayClientError::Transport("invalid workload token".into()))?;
    request
        .metadata_mut()
        .insert("authorization", authorization);
    Ok(())
}

fn rpc_error(status: tonic::Status) -> McpGatewayClientError {
    McpGatewayClientError::Rpc {
        code: status.code(),
        message: status.message().to_owned(),
    }
}

/// The federated Tools one Run may reach, plus what it froze them at.
///
/// A per-Run registry rather than additions to the Worker's own. A frozen
/// catalog is a property of a Run, not of the Worker: two Runs against the same
/// server can legitimately hold different digests, one having frozen before a
/// change and one after, and a name-keyed registry shared between them could
/// only hold one. Registering into the shared one would also make the second Run
/// on a Worker fail with a duplicate-tool error for no reason it could act on.
pub struct FederatedRunTools {
    /// The Worker's native Tools plus this Run's federated ones.
    pub registry: agent_kernel::ToolRegistry,
    /// Definitions for the federated Tools, for offering them to the model.
    pub definitions: Vec<crate::WorkerToolDefinition>,
    /// Server name -> the catalog digest this Run froze, presented on each call.
    pub frozen_digests: std::collections::BTreeMap<String, String>,
    /// Servers that could not be discovered, with why.
    ///
    /// Reported rather than fatal, and rather than dropped. One unreachable
    /// third-party server should not fail a Run that may not even use it; but a
    /// Run silently missing tools it was configured with is the kind of thing
    /// that produces an unexplainable transcript, so the caller is told.
    pub unavailable: Vec<(String, String)>,
}

/// Discovers every MCP server the command carries and builds this Run's Tools.
///
/// Discovery happens once, here, at the start of the Run. Nothing re-discovers
/// later: the digest frozen now is what every call presents, so a server that
/// changes its catalog mid-Run does not change what the Run may do.
pub async fn discover_federated_tools(
    base_registry: &agent_kernel::ToolRegistry,
    client: &mut GrpcMcpFederationClient,
    command: &agent_protocol::RunExecutionCommand,
    workload_token: &str,
) -> FederatedRunTools {
    let mut registry = base_registry.clone();
    let mut definitions = Vec::new();
    let mut frozen_digests = std::collections::BTreeMap::new();
    let mut unavailable = Vec::new();

    for server in &command.mcp_servers {
        let catalog = match client
            .list_tools(command.tenant_id, command.run_id, server, workload_token)
            .await
        {
            Ok(catalog) => catalog,
            Err(error) => {
                unavailable.push((server.name.clone(), error.to_string()));
                continue;
            }
        };
        let discovered = catalog
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.qualified_name, tool.description, tool.input_schema));
        match crate::federated_tool_definitions(&server.name, &catalog.digest, discovered) {
            Ok(built) => {
                let mut registered = Vec::with_capacity(built.len());
                let mut rejected = None;
                for definition in built {
                    if let Err(error) = registry.register(definition.descriptor.clone()) {
                        rejected = Some(error.to_string());
                        break;
                    }
                    registered.push(definition);
                }
                match rejected {
                    // All or nothing per server. A half-registered catalog would
                    // freeze a digest that describes tools the Run cannot see.
                    Some(error) => unavailable.push((server.name.clone(), error)),
                    None => {
                        definitions.extend(registered);
                        frozen_digests.insert(server.name.clone(), catalog.digest);
                    }
                }
            }
            Err(error) => unavailable.push((server.name.clone(), error.to_string())),
        }
    }

    FederatedRunTools {
        registry,
        definitions,
        frozen_digests,
        unavailable,
    }
}
