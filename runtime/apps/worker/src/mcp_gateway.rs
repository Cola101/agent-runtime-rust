//! Worker-side client for federated MCP calls (ADR-0040).
//!
//! The Worker never opens a sealed credential and never reaches an MCP server
//! directly. It hands the sealed envelope to the gateway, which opens it and
//! makes the call. What travels back is a bounded result.

use agent_model_gateway_protocol::mcp_server_authorization_digest;
use agent_model_gateway_protocol::v1::mcp_federation_client::McpFederationClient as GrpcMcpFederationStub;
use agent_model_gateway_protocol::v1::{
    McpCallToolRequest, McpListToolsRequest, McpServerRef as WireServerRef,
};
use agent_protocol::{McpElicitationRequest, McpInputContinuation, McpServerSnapshot};
use futures_util::{StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

const LEGACY_SCHEMA_VERSION: u32 = 1;
const COMPLETE_IDENTITY_SCHEMA_VERSION: u32 = 2;
const DEFAULT_SHARED_DISCOVERY_CONCURRENCY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpDiscoveryPolicy {
    pub max_concurrent: NonZeroUsize,
    pub per_server_timeout: Duration,
    pub total_timeout: Duration,
    pub max_attempts_per_server: u8,
    pub initial_retry_backoff: Duration,
}

impl Default for McpDiscoveryPolicy {
    fn default() -> Self {
        Self {
            max_concurrent: NonZeroUsize::new(4).expect("four is non-zero"),
            per_server_timeout: Duration::from_secs(3),
            total_timeout: Duration::from_secs(10),
            max_attempts_per_server: 2,
            initial_retry_backoff: Duration::from_millis(100),
        }
    }
}

impl McpDiscoveryPolicy {
    fn frozen_for(command: &agent_protocol::RunExecutionCommand) -> Option<Self> {
        let snapshot = command.runtime_policy.as_ref()?.mcp_discovery.clone();
        Some(Self {
            max_concurrent: NonZeroUsize::new(snapshot.max_concurrent_servers.into())?,
            per_server_timeout: Duration::from_millis(snapshot.per_server_timeout_ms),
            total_timeout: Duration::from_millis(snapshot.total_timeout_ms),
            max_attempts_per_server: snapshot.max_attempts_per_server,
            initial_retry_backoff: Duration::from_millis(snapshot.initial_retry_backoff_ms),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerHealth {
    Ready,
    Unavailable,
}

/// One server's observable discovery outcome. A missing optional server may
/// let the Run continue, but it must never disappear from diagnostics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpServerDiscoveryStatus {
    pub server_name: String,
    pub required: bool,
    pub health: McpServerHealth,
    pub attempts: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Process-local admission for MCP discovery calls shared by every clone of a
/// gateway client.
///
/// Per-Run concurrency remains part of the frozen execution policy. This is a
/// separate operational ceiling: it prevents many individually-valid Runs
/// from overwhelming one Worker and rotates queued admissions by tenant so a
/// noisy tenant cannot consume the whole queue.
#[derive(Clone)]
pub struct McpDiscoveryScheduler {
    inner: Arc<McpDiscoverySchedulerInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct McpDiscoverySchedulerSnapshot {
    pub max_in_flight: usize,
    pub in_flight: usize,
    pub queued_tenants: usize,
    pub queued_requests: usize,
}

struct McpDiscoverySchedulerInner {
    max_in_flight: usize,
    state: Mutex<McpDiscoverySchedulerState>,
}

#[derive(Default)]
struct McpDiscoverySchedulerState {
    in_flight: usize,
    tenant_order: VecDeque<Uuid>,
    pending: HashMap<Uuid, VecDeque<oneshot::Sender<McpDiscoveryPermit>>>,
}

struct McpDiscoveryPermit {
    inner: Option<Arc<McpDiscoverySchedulerInner>>,
}

impl McpDiscoveryScheduler {
    pub fn new(max_in_flight: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(McpDiscoverySchedulerInner {
                max_in_flight: max_in_flight.get(),
                state: Mutex::new(McpDiscoverySchedulerState::default()),
            }),
        }
    }

    /// Low-cardinality host observability; tenant identifiers never leave the
    /// scheduler, while operators can still see saturation and queue growth.
    #[must_use]
    pub fn snapshot(&self) -> McpDiscoverySchedulerSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .expect("MCP discovery scheduler lock poisoned");
        McpDiscoverySchedulerSnapshot {
            max_in_flight: self.inner.max_in_flight,
            in_flight: state.in_flight,
            queued_tenants: state.pending.len(),
            queued_requests: state.pending.values().map(VecDeque::len).sum(),
        }
    }

    async fn acquire(&self, tenant_id: Uuid) -> McpDiscoveryPermit {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("MCP discovery scheduler lock poisoned");
            let queue = state.pending.entry(tenant_id).or_default();
            if queue.is_empty() {
                state.tenant_order.push_back(tenant_id);
            }
            state
                .pending
                .get_mut(&tenant_id)
                .expect("tenant queue was just inserted")
                .push_back(sender);
            Self::dispatch(&self.inner, &mut state);
        }
        receiver
            .await
            .expect("MCP discovery scheduler dropped a queued admission")
    }

    fn dispatch(inner: &Arc<McpDiscoverySchedulerInner>, state: &mut McpDiscoverySchedulerState) {
        while state.in_flight < inner.max_in_flight {
            let Some(tenant_id) = state.tenant_order.pop_front() else {
                break;
            };
            let (sender, has_more) = {
                let queue = state
                    .pending
                    .get_mut(&tenant_id)
                    .expect("scheduled tenant must have a queue");
                let sender = queue
                    .pop_front()
                    .expect("scheduled tenant queue must not be empty");
                (sender, !queue.is_empty())
            };
            if has_more {
                state.tenant_order.push_back(tenant_id);
            } else {
                state.pending.remove(&tenant_id);
            }

            let permit = McpDiscoveryPermit {
                inner: Some(Arc::clone(inner)),
            };
            match sender.send(permit) {
                Ok(()) => state.in_flight += 1,
                Err(mut cancelled) => {
                    // The waiting Run hit its total deadline. Disarm the
                    // undelivered permit while the state lock is held, then
                    // continue directly to the next tenant.
                    cancelled.inner.take();
                }
            }
        }
    }
}

impl Default for McpDiscoveryScheduler {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_SHARED_DISCOVERY_CONCURRENCY)
                .expect("default discovery concurrency is non-zero"),
        )
    }
}

impl Drop for McpDiscoveryPermit {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let mut state = inner
            .state
            .lock()
            .expect("MCP discovery scheduler lock poisoned");
        state.in_flight = state
            .in_flight
            .checked_sub(1)
            .expect("MCP discovery permit released without admission");
        McpDiscoveryScheduler::dispatch(&inner, &mut state);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpGatewayClientError {
    #[error("mcp gateway transport failed: {0}")]
    Transport(String),
    #[error("mcp gateway RPC failed with {code}: {message}")]
    Rpc { code: Code, message: String },
    #[error("mcp gateway returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("mcp request was cancelled")]
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct McpCallContext {
    pub cancellation: CancellationToken,
    pub progress: tokio::sync::mpsc::Sender<McpProgressNotification>,
    pub progress_token: String,
    pub cancellation_reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpProgressNotification {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpToolRoundOutcome {
    Complete {
        content: serde_json::Value,
        is_error: bool,
    },
    InputRequired {
        round: u8,
        request_state: String,
        requests: BTreeMap<String, McpElicitationRequest>,
    },
}

#[derive(serde::Deserialize)]
struct WireMcpRoundTripRequired {
    round: u8,
    request_state: String,
    requests: BTreeMap<String, McpElicitationRequest>,
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

/// The Run identity every federation call is bound to.
///
/// Taken from the execution command, never assembled by a caller. The gateway
/// checks all five against the signed workload token, so a request cannot name a
/// tenant it was not issued for -- which is exactly what it could do when only
/// tenant_id travelled and nothing verified it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FederationIdentity {
    pub tenant_id: Uuid,
    pub application_id: Uuid,
    pub workload_identity_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub agent_version_id: Uuid,
    pub attempt_id: Uuid,
    pub worker_id: Uuid,
    pub worker_incarnation_id: Uuid,
}

impl FederationIdentity {
    pub fn from_command(command: &agent_protocol::RunExecutionCommand) -> Self {
        Self {
            tenant_id: command.tenant_id,
            application_id: command.application_id,
            workload_identity_id: command.workload_identity_id,
            run_id: command.run_id,
            session_id: command.session_id,
            workspace_id: command.workspace_id,
            agent_version_id: command.agent_version_id,
            attempt_id: command.attempt_id,
            worker_id: command.worker_id,
            worker_incarnation_id: command.worker_incarnation_id,
        }
    }

    fn wire_schema_version(&self) -> u32 {
        if [
            self.application_id,
            self.workload_identity_id,
            self.session_id,
            self.workspace_id,
            self.agent_version_id,
        ]
        .iter()
        .all(|id| !id.is_nil())
        {
            COMPLETE_IDENTITY_SCHEMA_VERSION
        } else {
            LEGACY_SCHEMA_VERSION
        }
    }

    fn wire_complete_identity_field(&self, value: Uuid) -> String {
        if self.wire_schema_version() == COMPLETE_IDENTITY_SCHEMA_VERSION {
            optional_uuid(value)
        } else {
            String::new()
        }
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

/// Protocol-neutral MCP operations consumed by discovery and Tool execution.
/// Implementations own their trust boundary: the cloud implementation calls a
/// credential-holding gRPC Gateway, while an in-process local Host can use a
/// credential-free backend without pretending a network identity was checked.
pub trait McpFederationBackend: Send + Sync {
    fn list_tools<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        workload_token: &'a str,
    ) -> BoxFuture<'a, Result<DiscoveredCatalog, McpGatewayClientError>>;

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        workload_token: &'a str,
        context: &'a McpCallContext,
    ) -> BoxFuture<'a, Result<(serde_json::Value, bool), McpGatewayClientError>>;

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool_round<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        workload_token: &'a str,
        context: &'a McpCallContext,
        continuation: Option<&'a McpInputContinuation>,
    ) -> BoxFuture<'a, Result<McpToolRoundOutcome, McpGatewayClientError>> {
        Box::pin(async move {
            if continuation.is_some() {
                return Err(McpGatewayClientError::InvalidResponse(
                    "MCP backend has no recoverable continuation path".into(),
                ));
            }
            let (content, is_error) = self
                .call_tool(
                    identity,
                    server,
                    qualified_name,
                    arguments,
                    frozen_catalog_digest,
                    workload_token,
                    context,
                )
                .await?;
            Ok(McpToolRoundOutcome::Complete { content, is_error })
        })
    }
}

#[derive(Clone)]
struct GrpcMcpFederationBackend {
    inner: GrpcMcpFederationStub<Channel>,
}

/// Cloneable client capability shared by the Kernel-facing MCP path. The
/// backend is type-erased so the Coordinator is independent of gRPC.
#[derive(Clone)]
pub struct McpFederationClient {
    backend: Arc<dyn McpFederationBackend>,
    discovery_scheduler: McpDiscoveryScheduler,
}

/// Compatibility name for cloud callers. New protocol-neutral code should use
/// `McpFederationClient`.
pub type GrpcMcpFederationClient = McpFederationClient;

impl McpFederationClient {
    pub async fn connect(endpoint: String) -> Result<Self, McpGatewayClientError> {
        let inner = GrpcMcpFederationStub::connect(endpoint)
            .await
            .map_err(|error| McpGatewayClientError::Transport(error.to_string()))?;
        Ok(Self::from_backend(GrpcMcpFederationBackend { inner }))
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
        Ok(Self::from_backend(GrpcMcpFederationBackend {
            inner: GrpcMcpFederationStub::new(channel),
        }))
    }

    #[must_use]
    pub fn from_backend(backend: impl McpFederationBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
            discovery_scheduler: McpDiscoveryScheduler::default(),
        }
    }

    /// Replaces the default Worker-wide discovery ceiling. Every clone made
    /// from the returned client shares this scheduler.
    pub fn with_discovery_scheduler(mut self, scheduler: McpDiscoveryScheduler) -> Self {
        self.discovery_scheduler = scheduler;
        self
    }

    pub async fn list_tools(
        &self,
        identity: &FederationIdentity,
        server: &McpServerSnapshot,
        workload_token: &str,
    ) -> Result<DiscoveredCatalog, McpGatewayClientError> {
        self.backend
            .list_tools(identity, server, workload_token)
            .await
    }

    pub async fn call_tool(
        &self,
        identity: &FederationIdentity,
        server: &McpServerSnapshot,
        qualified_name: &str,
        arguments: &serde_json::Value,
        frozen_catalog_digest: &str,
        workload_token: &str,
    ) -> Result<(serde_json::Value, bool), McpGatewayClientError> {
        let (progress, _receiver) = tokio::sync::mpsc::channel(1);
        self.call_tool_with_context(
            identity,
            server,
            qualified_name,
            arguments,
            frozen_catalog_digest,
            workload_token,
            &McpCallContext {
                cancellation: CancellationToken::new(),
                progress,
                progress_token: format!("{}:{qualified_name}", identity.attempt_id),
                cancellation_reason: "MCP request was abandoned".into(),
            },
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    pub async fn call_tool_with_context(
        &self,
        identity: &FederationIdentity,
        server: &McpServerSnapshot,
        qualified_name: &str,
        arguments: &serde_json::Value,
        frozen_catalog_digest: &str,
        workload_token: &str,
        context: &McpCallContext,
    ) -> Result<(serde_json::Value, bool), McpGatewayClientError> {
        self.backend
            .call_tool(
                identity,
                server,
                qualified_name,
                arguments,
                frozen_catalog_digest,
                workload_token,
                context,
            )
            .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    pub async fn call_tool_round_with_context(
        &self,
        identity: &FederationIdentity,
        server: &McpServerSnapshot,
        qualified_name: &str,
        arguments: &serde_json::Value,
        frozen_catalog_digest: &str,
        workload_token: &str,
        context: &McpCallContext,
        continuation: Option<&McpInputContinuation>,
    ) -> Result<McpToolRoundOutcome, McpGatewayClientError> {
        self.backend
            .call_tool_round(
                identity,
                server,
                qualified_name,
                arguments,
                frozen_catalog_digest,
                workload_token,
                context,
                continuation,
            )
            .await
    }
}

impl McpFederationBackend for GrpcMcpFederationBackend {
    fn list_tools<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        workload_token: &'a str,
    ) -> BoxFuture<'a, Result<DiscoveredCatalog, McpGatewayClientError>> {
        Box::pin(async move {
            let mut inner = self.inner.clone();
            let mut request = tonic::Request::new(McpListToolsRequest {
                schema_version: identity.wire_schema_version(),
                tenant_id: identity.tenant_id.to_string(),
                application_id: identity.wire_complete_identity_field(identity.application_id),
                workload_identity_id: identity
                    .wire_complete_identity_field(identity.workload_identity_id),
                run_id: identity.run_id.to_string(),
                session_id: identity.wire_complete_identity_field(identity.session_id),
                workspace_id: identity.wire_complete_identity_field(identity.workspace_id),
                agent_version_id: identity.wire_complete_identity_field(identity.agent_version_id),
                attempt_id: identity.attempt_id.to_string(),
                worker_id: identity.worker_id.to_string(),
                worker_incarnation_id: identity.worker_incarnation_id.to_string(),
                server: Some(wire_server(server)?),
            });
            authorize(&mut request, workload_token)?;
            let response = inner.list_tools(request).await.map_err(rpc_error)?;
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
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        workload_token: &'a str,
        context: &'a McpCallContext,
    ) -> BoxFuture<'a, Result<(serde_json::Value, bool), McpGatewayClientError>> {
        Box::pin(async move {
            let mut inner = self.inner.clone();
            let mut request = tonic::Request::new(McpCallToolRequest {
                schema_version: identity.wire_schema_version(),
                tenant_id: identity.tenant_id.to_string(),
                application_id: identity.wire_complete_identity_field(identity.application_id),
                workload_identity_id: identity
                    .wire_complete_identity_field(identity.workload_identity_id),
                run_id: identity.run_id.to_string(),
                session_id: identity.wire_complete_identity_field(identity.session_id),
                workspace_id: identity.wire_complete_identity_field(identity.workspace_id),
                agent_version_id: identity.wire_complete_identity_field(identity.agent_version_id),
                attempt_id: identity.attempt_id.to_string(),
                worker_id: identity.worker_id.to_string(),
                worker_incarnation_id: identity.worker_incarnation_id.to_string(),
                server: Some(wire_server(server)?),
                qualified_name: qualified_name.to_owned(),
                arguments_json: arguments.to_string().into_bytes(),
                frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                input_continuation_json: Vec::new(),
            });
            authorize(&mut request, workload_token)?;
            let response = tokio::select! {
                biased;
                () = context.cancellation.cancelled() => {
                    return Err(McpGatewayClientError::Cancelled);
                }
                response = inner.call_tool(request) => response.map_err(rpc_error)?,
            };
            let response = response.into_inner();
            if !response.input_required_json.is_empty() {
                return Err(McpGatewayClientError::InvalidResponse(
                    "MCP Tool requires user input but this caller has no continuation path".into(),
                ));
            }
            let content = std::str::from_utf8(&response.content_json)
                .map_err(|_| McpGatewayClientError::InvalidResponse("content is not utf-8".into()))
                .and_then(|text| {
                    serde_json::from_str::<serde_json::Value>(text)
                        .map_err(|error| McpGatewayClientError::InvalidResponse(error.to_string()))
                })?;
            Ok((content, response.is_error))
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool_round<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        workload_token: &'a str,
        context: &'a McpCallContext,
        continuation: Option<&'a McpInputContinuation>,
    ) -> BoxFuture<'a, Result<McpToolRoundOutcome, McpGatewayClientError>> {
        Box::pin(async move {
            let input_continuation_json = continuation
                .map(serde_json::to_vec)
                .transpose()
                .map_err(|error| McpGatewayClientError::InvalidResponse(error.to_string()))?
                .unwrap_or_default();
            let mut inner = self.inner.clone();
            let mut request = tonic::Request::new(McpCallToolRequest {
                schema_version: identity.wire_schema_version(),
                tenant_id: identity.tenant_id.to_string(),
                application_id: identity.wire_complete_identity_field(identity.application_id),
                workload_identity_id: identity
                    .wire_complete_identity_field(identity.workload_identity_id),
                run_id: identity.run_id.to_string(),
                session_id: identity.wire_complete_identity_field(identity.session_id),
                workspace_id: identity.wire_complete_identity_field(identity.workspace_id),
                agent_version_id: identity.wire_complete_identity_field(identity.agent_version_id),
                attempt_id: identity.attempt_id.to_string(),
                worker_id: identity.worker_id.to_string(),
                worker_incarnation_id: identity.worker_incarnation_id.to_string(),
                server: Some(wire_server(server)?),
                qualified_name: qualified_name.to_owned(),
                arguments_json: arguments.to_string().into_bytes(),
                frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                input_continuation_json,
            });
            authorize(&mut request, workload_token)?;
            let response = tokio::select! {
                biased;
                () = context.cancellation.cancelled() => {
                    return Err(McpGatewayClientError::Cancelled);
                }
                response = inner.call_tool(request) => response.map_err(rpc_error)?,
            }
            .into_inner();
            if !response.input_required_json.is_empty() {
                if !response.content_json.is_empty() || response.is_error {
                    return Err(McpGatewayClientError::InvalidResponse(
                        "MCP gateway returned two Tool round outcomes".into(),
                    ));
                }
                let required: WireMcpRoundTripRequired =
                    serde_json::from_slice(&response.input_required_json).map_err(|error| {
                        McpGatewayClientError::InvalidResponse(error.to_string())
                    })?;
                return Ok(McpToolRoundOutcome::InputRequired {
                    round: required.round,
                    request_state: required.request_state,
                    requests: required.requests,
                });
            }
            let content = std::str::from_utf8(&response.content_json)
                .map_err(|_| McpGatewayClientError::InvalidResponse("content is not utf-8".into()))
                .and_then(|text| {
                    serde_json::from_str::<serde_json::Value>(text)
                        .map_err(|error| McpGatewayClientError::InvalidResponse(error.to_string()))
                })?;
            Ok(McpToolRoundOutcome::Complete {
                content,
                is_error: response.is_error,
            })
        })
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
        credential_envelope_json: envelope,
        protocol_revision: server.protocol_revision.as_str().to_owned(),
        client_capabilities: server
            .client_capabilities
            .iter()
            .map(|capability| match capability {
                agent_protocol::McpClientCapability::Elicitation => "elicitation".to_owned(),
            })
            .collect(),
    })
}

pub(crate) fn authorized_server_digests(
    servers: &[McpServerSnapshot],
) -> Result<BTreeMap<Uuid, String>, McpGatewayClientError> {
    servers
        .iter()
        .map(|server| {
            let wire = wire_server(server)?;
            Ok((server.server_id, mcp_server_authorization_digest(&wire)))
        })
        .collect()
}

fn optional_uuid(value: Uuid) -> String {
    if value.is_nil() {
        String::new()
    } else {
        value.to_string()
    }
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
    /// The scheduling/deadline policy that produced this catalog. Recovery
    /// binds it alongside the Tool digests so a Run cannot silently resume
    /// under different discovery semantics.
    pub policy: McpDiscoveryPolicy,
    /// Servers that could not be discovered, with why.
    ///
    /// Reported rather than fatal, and rather than dropped. One unreachable
    /// third-party server should not fail a Run that may not even use it; but a
    /// Run silently missing tools it was configured with is the kind of thing
    /// that produces an unexplainable transcript, so the caller is told.
    pub unavailable: Vec<(String, String)>,
    /// Ready and unavailable entries in command order, including optional
    /// failures that did not reject the Run.
    pub statuses: Vec<McpServerDiscoveryStatus>,
}

/// Attaches an already-discovered MCP catalog to one accepted or restored Run.
///
/// This is deliberately independent from NATS: a native runtime host can
/// discover, restore and attach using the same kernel path. `WorkerProcessor`
/// performs the checkpoint-catalog comparison before accepting the attachment.
pub fn attach_discovered_federated_tools(
    processor: &mut crate::WorkerProcessor,
    client: GrpcMcpFederationClient,
    command: &agent_protocol::RunExecutionCommand,
    attempt_id: Uuid,
    discovered: FederatedRunTools,
) -> Result<(), crate::WorkerAssignmentError> {
    let required_failures = discovered
        .statuses
        .iter()
        .filter(|status| status.required && status.health == McpServerHealth::Unavailable)
        .map(|status| {
            format!(
                "{} after {} attempt(s): {}",
                status.server_name,
                status.attempts,
                status.error.as_deref().unwrap_or("unavailable")
            )
        })
        .collect::<Vec<_>>();
    if !required_failures.is_empty() {
        return Err(crate::WorkerAssignmentError::RequiredMcpServersUnavailable(
            required_failures.join("; "),
        ));
    }
    let identity = FederationIdentity::from_command(command);
    let mut executors: Vec<(String, std::sync::Arc<dyn agent_tool_runtime::ToolExecutor>)> =
        Vec::new();
    for definition in &discovered.definitions {
        let Some(server_name) = crate::federated_server_of(&definition.descriptor.name) else {
            continue;
        };
        let Some(server) = command
            .mcp_servers
            .iter()
            .find(|candidate| candidate.name == server_name)
        else {
            continue;
        };
        let Some(digest) = discovered.frozen_digests.get(server_name) else {
            continue;
        };
        executors.push((
            definition.descriptor.name.clone(),
            std::sync::Arc::new(FederatedToolExecutor::new(
                client.clone(),
                server.clone(),
                identity,
                digest.clone(),
                command.workload_token.as_str().to_owned(),
            )) as std::sync::Arc<dyn agent_tool_runtime::ToolExecutor>,
        ));
    }
    processor.attach_federated_tools(
        attempt_id,
        discovered.registry,
        discovered.definitions,
        executors,
        discovered.policy,
    )
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
    discover_federated_tools_with_policy(
        base_registry,
        client,
        command,
        workload_token,
        McpDiscoveryPolicy::frozen_for(command).unwrap_or_default(),
    )
    .await
}

pub async fn discover_federated_tools_with_policy(
    base_registry: &agent_kernel::ToolRegistry,
    client: &mut GrpcMcpFederationClient,
    command: &agent_protocol::RunExecutionCommand,
    workload_token: &str,
    policy: McpDiscoveryPolicy,
) -> FederatedRunTools {
    // A v10+ command is authoritative. The explicit policy argument remains for
    // legacy commands and focused tests, but must not become an override that
    // lets a Host silently reinterpret an already-frozen Run.
    let policy = McpDiscoveryPolicy::frozen_for(command).unwrap_or(policy);
    let mut registry = base_registry.clone();
    let mut definitions = Vec::new();
    let mut frozen_digests = std::collections::BTreeMap::new();
    let mut unavailable = Vec::new();

    let identity = FederationIdentity::from_command(command);
    let mut discoveries = Box::pin(
        futures_util::stream::iter(command.mcp_servers.iter().cloned().enumerate().map(
            |(ordinal, server)| {
                let client = client.clone();
                let scheduler = client.discovery_scheduler.clone();
                async move {
                    let deadline = tokio::time::Instant::now() + policy.per_server_timeout;
                    let max_attempts = policy.max_attempts_per_server.max(1);
                    let mut attempt = 0_u8;
                    loop {
                        attempt = attempt.saturating_add(1);
                        // Admission covers network work, not retry sleep. A
                        // failing tenant must rejoin the fair queue instead of
                        // holding a shared slot while backing off.
                        let admission = scheduler.acquire(identity.tenant_id).await;
                        let catalog = match tokio::time::timeout_at(
                            deadline,
                            client.list_tools(&identity, &server, workload_token),
                        )
                        .await
                        {
                            Ok(catalog) => catalog,
                            Err(_) => Err(McpGatewayClientError::Transport(
                                "MCP discovery deadline exceeded".into(),
                            )),
                        };
                        drop(admission);

                        let retryable = catalog
                            .as_ref()
                            .is_err_and(McpGatewayClientError::is_retryable);
                        if !retryable || attempt >= max_attempts {
                            break (ordinal, server, attempt, catalog);
                        }
                        let multiplier = 1_u32 << u32::from(attempt.saturating_sub(1).min(15));
                        let delay = policy.initial_retry_backoff.saturating_mul(multiplier);
                        let Some(wake_at) = tokio::time::Instant::now().checked_add(delay) else {
                            break (ordinal, server, attempt, catalog);
                        };
                        if wake_at >= deadline {
                            break (ordinal, server, attempt, catalog);
                        }
                        tokio::time::sleep_until(wake_at).await;
                    }
                }
            },
        ))
        .buffer_unordered(policy.max_concurrent.get()),
    );
    let total_deadline = tokio::time::Instant::now() + policy.total_timeout;
    let mut discovered_servers = Vec::with_capacity(command.mcp_servers.len());
    let total_budget_exceeded = loop {
        match tokio::time::timeout_at(total_deadline, discoveries.next()).await {
            Ok(Some(discovered)) => discovered_servers.push(discovered),
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    // Dropping the stream cancels every still-running gRPC future before the
    // partial result is processed or exposed to the model.
    drop(discoveries);
    if total_budget_exceeded {
        let completed = discovered_servers
            .iter()
            .map(|(ordinal, _, _, _)| *ordinal)
            .collect::<std::collections::BTreeSet<_>>();
        discovered_servers.extend(command.mcp_servers.iter().cloned().enumerate().filter_map(
            |(ordinal, server)| {
                (!completed.contains(&ordinal)).then(|| {
                    (
                        ordinal,
                        server,
                        0,
                        Err(McpGatewayClientError::Transport(
                            "MCP total discovery budget exceeded".into(),
                        )),
                    )
                })
            },
        ));
    }
    // Network completion order must not change the model-visible Tool order or
    // which duplicate-name conflict wins. The command is the stable authority.
    discovered_servers.sort_by_key(|(ordinal, _, _, _)| *ordinal);

    let mut statuses = Vec::with_capacity(command.mcp_servers.len());
    for (_, server, attempts, catalog) in discovered_servers {
        let catalog = match catalog {
            Ok(catalog) => catalog,
            Err(error) => {
                let error = error.to_string();
                statuses.push(McpServerDiscoveryStatus {
                    server_name: server.name.clone(),
                    required: server.required,
                    health: McpServerHealth::Unavailable,
                    attempts,
                    error: Some(error.clone()),
                });
                unavailable.push((server.name.clone(), error));
                continue;
            }
        };
        let discovered = catalog
            .tools
            .iter()
            .cloned()
            .map(|tool| (tool.qualified_name, tool.description, tool.input_schema));
        match crate::federated_tool_definitions(
            &server.name,
            &catalog.digest,
            discovered,
            &server.tool_effect_overrides,
        ) {
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
                    Some(error) => {
                        statuses.push(McpServerDiscoveryStatus {
                            server_name: server.name.clone(),
                            required: server.required,
                            health: McpServerHealth::Unavailable,
                            attempts,
                            error: Some(error.clone()),
                        });
                        unavailable.push((server.name.clone(), error));
                    }
                    None => {
                        definitions.extend(registered);
                        frozen_digests.insert(server.name.clone(), catalog.digest);
                        statuses.push(McpServerDiscoveryStatus {
                            server_name: server.name,
                            required: server.required,
                            health: McpServerHealth::Ready,
                            attempts,
                            error: None,
                        });
                    }
                }
            }
            Err(error) => {
                let error = error.to_string();
                statuses.push(McpServerDiscoveryStatus {
                    server_name: server.name.clone(),
                    required: server.required,
                    health: McpServerHealth::Unavailable,
                    attempts,
                    error: Some(error.clone()),
                });
                unavailable.push((server.name.clone(), error));
            }
        }
    }

    FederatedRunTools {
        registry,
        definitions,
        frozen_digests,
        policy,
        unavailable,
        statuses,
    }
}

/// Executes one federated tool by asking the gateway to call it.
///
/// A `ToolExecutor` like any other, which is the point: approval, the started
/// and result events, the checkpoint and the transcript all work unchanged. A
/// special case in the dispatch path would have to reimplement each of those and
/// would drift from the native path the first time one of them changed.
pub struct FederatedToolExecutor {
    client: tokio::sync::Mutex<GrpcMcpFederationClient>,
    server: McpServerSnapshot,
    identity: FederationIdentity,
    /// Doubles as the implementation digest, so a Checkpoint restore that
    /// recomputes the catalog digest refuses a Run whose server moved.
    frozen_catalog_digest: String,
    workload_token: String,
}

impl FederatedToolExecutor {
    pub fn new(
        client: GrpcMcpFederationClient,
        server: McpServerSnapshot,
        identity: FederationIdentity,
        frozen_catalog_digest: String,
        workload_token: String,
    ) -> Self {
        Self {
            client: tokio::sync::Mutex::new(client),
            server,
            identity,
            frozen_catalog_digest,
            workload_token,
        }
    }

    async fn execute_round(
        &self,
        request: agent_protocol::ToolExecutionRequest,
        context: agent_tool_runtime::ToolExecutionContext,
        continuation: Option<agent_protocol::McpInputContinuation>,
        progress: agent_tool_runtime::ToolProgressReporter,
    ) -> Result<agent_tool_runtime::ToolExecutionResult, agent_tool_runtime::ToolExecutionError>
    {
        use agent_tool_runtime::{ToolExecutionError, ToolExecutionProgress, ToolExecutionResult};
        if request.sandbox != agent_protocol::SandboxClass::Federated {
            return Err(ToolExecutionError::WrongSandbox);
        }
        if context.tenant_id != self.identity.tenant_id || context.run_id != self.identity.run_id {
            return Err(ToolExecutionError::InvalidContext(
                "federated executor belongs to another run".into(),
            ));
        }
        let client = self.client.lock().await;
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(32);
        let call_context = McpCallContext {
            cancellation: context.cancellation.clone(),
            progress: progress_tx,
            progress_token: format!("{}:{}", context.attempt_id, request.call.id),
            cancellation_reason: "run cancellation requested".into(),
        };
        let call = client.call_tool_round_with_context(
            &self.identity,
            &self.server,
            &request.call.name,
            &request.call.arguments,
            &self.frozen_catalog_digest,
            &self.workload_token,
            &call_context,
            continuation.as_ref(),
        );
        tokio::pin!(call);
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut call => break outcome,
                update = progress_rx.recv() => {
                    let Some(update) = update else {
                        continue;
                    };
                    progress.try_report(ToolExecutionProgress {
                        progress: update.progress,
                        total: update.total,
                        message: update.message,
                    });
                }
            }
        };
        match outcome {
            Ok(McpToolRoundOutcome::Complete { content, is_error }) => Ok(ToolExecutionResult {
                content,
                is_error,
                exit_code: i32::from(is_error),
            }),
            Ok(McpToolRoundOutcome::InputRequired {
                round,
                request_state,
                requests,
            }) => Err(ToolExecutionError::McpInputRequired {
                round,
                request_state,
                requests,
            }),
            Err(McpGatewayClientError::Cancelled) => Err(ToolExecutionError::Cancelled),
            Err(error) => Err(ToolExecutionError::Engine(error.to_string())),
        }
    }
}

impl agent_tool_runtime::ToolExecutor for FederatedToolExecutor {
    fn implementation_digest(&self) -> &str {
        &self.frozen_catalog_digest
    }

    fn execute(
        &self,
        request: agent_protocol::ToolExecutionRequest,
        context: agent_tool_runtime::ToolExecutionContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        agent_tool_runtime::ToolExecutionResult,
                        agent_tool_runtime::ToolExecutionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        self.execute_with_progress(
            request,
            context,
            agent_tool_runtime::ToolProgressReporter::disabled(),
        )
    }

    fn execute_with_progress(
        &self,
        request: agent_protocol::ToolExecutionRequest,
        context: agent_tool_runtime::ToolExecutionContext,
        progress: agent_tool_runtime::ToolProgressReporter,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        agent_tool_runtime::ToolExecutionResult,
                        agent_tool_runtime::ToolExecutionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self.execute_round(request, context, None, progress))
    }

    fn resume_with_mcp_input(
        &self,
        request: agent_protocol::ToolExecutionRequest,
        context: agent_tool_runtime::ToolExecutionContext,
        continuation: agent_protocol::McpInputContinuation,
        progress: agent_tool_runtime::ToolProgressReporter,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        agent_tool_runtime::ToolExecutionResult,
                        agent_tool_runtime::ToolExecutionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self.execute_round(request, context, Some(continuation), progress))
    }
}
