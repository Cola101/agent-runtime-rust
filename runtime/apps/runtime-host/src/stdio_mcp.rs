use agent_model_gateway::mcp::{
    McpCatalog, McpFederationError, McpToolCallOutcome, McpToolResult,
    attach_modern_request_metadata, catalog_from_list_result, empty_catalog_for_capabilities,
    mrtr_responses_value, parse_modern_input_required, prompt_page_from_list_result,
    prompt_result_from_get_result, resource_page_from_list_result, resource_read_from_result,
    resource_template_page_from_list_result, tool_result_from_call_result,
};
use agent_protocol::{
    McpInputContinuation, McpPromptPage, McpPromptResult, McpProtocolRevision, McpResourcePage,
    McpResourceReadResult, McpResourceTemplatePage, McpServerCapability,
};
use agent_runtime_worker::{McpCallContext, McpProgressNotification};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{LocalMcpLifecycleConfig, LocalMcpLifecycleSnapshot, LocalStdioMcpConfig};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MODERN_MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_STDIO_MESSAGE_BYTES: usize = 256 * 1024;
const SESSION_QUEUE_CAPACITY: usize = 32;

#[derive(Debug)]
enum SessionOperation {
    Health,
    ListTools {
        server_name: String,
    },
    ListResources {
        server_name: String,
        frozen_catalog_digest: String,
        cursor: Option<String>,
    },
    ReadResource {
        server_name: String,
        frozen_catalog_digest: String,
        uri: String,
    },
    ListResourceTemplates {
        server_name: String,
        frozen_catalog_digest: String,
        cursor: Option<String>,
    },
    ListPrompts {
        server_name: String,
        frozen_catalog_digest: String,
        cursor: Option<String>,
    },
    GetPrompt {
        server_name: String,
        frozen_catalog_digest: String,
        name: String,
        arguments: Option<Value>,
    },
    CallTool {
        server_name: String,
        qualified_name: String,
        arguments: Value,
        frozen_catalog_digest: String,
        continuation: Option<McpInputContinuation>,
    },
}

#[derive(Debug)]
enum SessionResponse {
    Healthy,
    Catalog(McpCatalog),
    ResourcePage(McpResourcePage),
    ResourceRead(McpResourceReadResult),
    ResourceTemplatePage(McpResourceTemplatePage),
    PromptPage(McpPromptPage),
    Prompt(McpPromptResult),
    Tool(McpToolCallOutcome),
}

struct SessionRequest {
    operation: SessionOperation,
    cancellation: CancellationToken,
    progress: Option<mpsc::Sender<McpProgressNotification>>,
    progress_token: Option<String>,
    cancellation_reason: String,
    response: oneshot::Sender<Result<SessionResponse, McpFederationError>>,
}

struct SessionHandle {
    sender: mpsc::Sender<SessionRequest>,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    usage: Arc<SessionUsage>,
}

struct SessionUsage {
    active_leases: AtomicUsize,
    last_used_millis: AtomicU64,
    origin: Instant,
}

impl SessionUsage {
    fn new() -> Self {
        Self {
            active_leases: AtomicUsize::new(0),
            last_used_millis: AtomicU64::new(0),
            origin: Instant::now(),
        }
    }

    fn now_millis(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn acquire(self: &Arc<Self>) -> SessionLease {
        self.active_leases.fetch_add(1, Ordering::AcqRel);
        SessionLease {
            usage: Arc::clone(self),
        }
    }

    fn active(&self) -> usize {
        self.active_leases.load(Ordering::Acquire)
    }

    fn idle_for(&self) -> Duration {
        Duration::from_millis(
            self.now_millis()
                .saturating_sub(self.last_used_millis.load(Ordering::Acquire)),
        )
    }
}

struct SessionLease {
    usage: Arc<SessionUsage>,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        // Publish the new idle origin before releasing the final lease. A
        // sweeper that observes zero active requests must also observe this
        // timestamp and cannot evict based on time spent doing useful work.
        self.usage
            .last_used_millis
            .store(self.usage.now_millis(), Ordering::Release);
        self.usage.active_leases.fetch_sub(1, Ordering::AcqRel);
    }
}

struct CatalogCacheEntry {
    catalog: McpCatalog,
    published_at: Instant,
}

#[derive(Default)]
struct LifecycleMetrics {
    catalog_cache_hits: AtomicU64,
    catalog_cache_misses: AtomicU64,
    failed_session_retirements: AtomicU64,
    idle_evictions: AtomicU64,
    lru_evictions: AtomicU64,
}

struct ClientLifetime {
    shutdown: CancellationToken,
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
}

impl Drop for ClientLifetime {
    fn drop(&mut self) {
        self.shutdown.cancel();
        // Drop cannot await, but cancelling every actor is sufficient to wake
        // its biased select and start asynchronous process-group reaping. This
        // preserves the pre-existing safe `drop Host` path as well as the
        // stronger explicit `shutdown().await` path.
        if let Ok(sessions) = self.sessions.try_lock() {
            for handle in sessions.values() {
                handle.shutdown.cancel();
            }
        }
    }
}

struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

#[derive(Clone)]
pub(crate) struct StdioMcpClient {
    configs: Arc<HashMap<Uuid, LocalStdioMcpConfig>>,
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
    catalogs: Arc<Mutex<HashMap<Uuid, CatalogCacheEntry>>>,
    request_timeout: Duration,
    catalog_ttl: Duration,
    session_idle_ttl: Duration,
    sweep_interval: Duration,
    max_sessions: usize,
    sweeper: Arc<Mutex<Option<JoinHandle<()>>>>,
    lifetime: Arc<ClientLifetime>,
    metrics: Arc<LifecycleMetrics>,
}

impl StdioMcpClient {
    pub(crate) fn new(
        configs: HashMap<Uuid, LocalStdioMcpConfig>,
        request_timeout: Duration,
        lifecycle: LocalMcpLifecycleConfig,
    ) -> Self {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = CancellationToken::new();
        Self {
            configs: Arc::new(configs),
            sessions: Arc::clone(&sessions),
            catalogs: Arc::new(Mutex::new(HashMap::new())),
            request_timeout,
            catalog_ttl: lifecycle.catalog_ttl,
            session_idle_ttl: lifecycle.session_idle_ttl,
            sweep_interval: lifecycle.sweep_interval,
            max_sessions: lifecycle.max_sessions,
            sweeper: Arc::new(Mutex::new(None)),
            lifetime: Arc::new(ClientLifetime { shutdown, sessions }),
            metrics: Arc::new(LifecycleMetrics::default()),
        }
    }

    pub(crate) async fn list_tools(
        &self,
        server_id: Uuid,
        server_name: &str,
    ) -> Result<McpCatalog, McpFederationError> {
        let cached = if self.catalog_ttl.is_zero() {
            None
        } else {
            self.catalogs
                .lock()
                .await
                .get(&server_id)
                .filter(|entry| entry.published_at.elapsed() <= self.catalog_ttl)
                .map(|entry| entry.catalog.clone())
        };
        if let Some(catalog) = cached {
            return match self.request(server_id, SessionOperation::Health).await? {
                SessionResponse::Healthy => {
                    self.metrics
                        .catalog_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    Ok(catalog)
                }
                SessionResponse::Catalog(_)
                | SessionResponse::ResourcePage(_)
                | SessionResponse::ResourceRead(_)
                | SessionResponse::ResourceTemplatePage(_)
                | SessionResponse::PromptPage(_)
                | SessionResponse::Prompt(_)
                | SessionResponse::Tool(_) => Err(McpFederationError::Protocol(
                    "stdio MCP returned data to a health check".into(),
                )),
            };
        }
        self.metrics
            .catalog_cache_misses
            .fetch_add(1, Ordering::Relaxed);
        match self
            .request(
                server_id,
                SessionOperation::ListTools {
                    server_name: server_name.to_owned(),
                },
            )
            .await?
        {
            SessionResponse::Catalog(catalog) => {
                self.catalogs.lock().await.insert(
                    server_id,
                    CatalogCacheEntry {
                        catalog: catalog.clone(),
                        published_at: Instant::now(),
                    },
                );
                Ok(catalog)
            }
            SessionResponse::Healthy
            | SessionResponse::ResourcePage(_)
            | SessionResponse::ResourceRead(_)
            | SessionResponse::ResourceTemplatePage(_)
            | SessionResponse::PromptPage(_)
            | SessionResponse::Prompt(_)
            | SessionResponse::Tool(_) => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong list result".into(),
            )),
        }
    }

    pub(crate) async fn list_resources(
        &self,
        server_id: Uuid,
        server_name: &str,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpResourcePage, McpFederationError> {
        match self
            .request(
                server_id,
                SessionOperation::ListResources {
                    server_name: server_name.to_owned(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    cursor: cursor.map(str::to_owned),
                },
            )
            .await?
        {
            SessionResponse::ResourcePage(page) => Ok(page),
            _ => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong resources/list result".into(),
            )),
        }
    }

    pub(crate) async fn read_resource(
        &self,
        server_id: Uuid,
        server_name: &str,
        frozen_catalog_digest: &str,
        uri: &str,
    ) -> Result<McpResourceReadResult, McpFederationError> {
        match self
            .request(
                server_id,
                SessionOperation::ReadResource {
                    server_name: server_name.to_owned(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    uri: uri.to_owned(),
                },
            )
            .await?
        {
            SessionResponse::ResourceRead(result) => Ok(result),
            _ => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong resources/read result".into(),
            )),
        }
    }

    pub(crate) async fn list_resource_templates(
        &self,
        server_id: Uuid,
        server_name: &str,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpResourceTemplatePage, McpFederationError> {
        match self
            .request(
                server_id,
                SessionOperation::ListResourceTemplates {
                    server_name: server_name.to_owned(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    cursor: cursor.map(str::to_owned),
                },
            )
            .await?
        {
            SessionResponse::ResourceTemplatePage(page) => Ok(page),
            _ => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong resources/templates/list result".into(),
            )),
        }
    }

    pub(crate) async fn list_prompts(
        &self,
        server_id: Uuid,
        server_name: &str,
        frozen_catalog_digest: &str,
        cursor: Option<&str>,
    ) -> Result<McpPromptPage, McpFederationError> {
        match self
            .request(
                server_id,
                SessionOperation::ListPrompts {
                    server_name: server_name.to_owned(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    cursor: cursor.map(str::to_owned),
                },
            )
            .await?
        {
            SessionResponse::PromptPage(page) => Ok(page),
            _ => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong prompts/list result".into(),
            )),
        }
    }

    pub(crate) async fn get_prompt(
        &self,
        server_id: Uuid,
        server_name: &str,
        frozen_catalog_digest: &str,
        name: &str,
        arguments: Option<&Value>,
    ) -> Result<McpPromptResult, McpFederationError> {
        match self
            .request(
                server_id,
                SessionOperation::GetPrompt {
                    server_name: server_name.to_owned(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    name: name.to_owned(),
                    arguments: arguments.cloned(),
                },
            )
            .await?
        {
            SessionResponse::Prompt(result) => Ok(result),
            _ => Err(McpFederationError::Protocol(
                "stdio MCP returned the wrong prompts/get result".into(),
            )),
        }
    }

    #[cfg(test)]
    pub(crate) async fn call_tool(
        &self,
        server_id: Uuid,
        server_name: &str,
        qualified_name: &str,
        arguments: &Value,
        frozen_catalog_digest: &str,
    ) -> Result<McpToolResult, McpFederationError> {
        let (progress, _receiver) = mpsc::channel(1);
        self.call_tool_with_lifecycle(
            server_id,
            server_name,
            qualified_name,
            arguments,
            frozen_catalog_digest,
            &McpCallContext {
                cancellation: CancellationToken::new(),
                progress,
                progress_token: format!("standalone:{server_id}:{qualified_name}"),
                cancellation_reason: "MCP request was abandoned".into(),
            },
        )
        .await
    }

    pub(crate) async fn call_tool_with_lifecycle(
        &self,
        server_id: Uuid,
        server_name: &str,
        qualified_name: &str,
        arguments: &Value,
        frozen_catalog_digest: &str,
        context: &McpCallContext,
    ) -> Result<McpToolResult, McpFederationError> {
        match self
            .call_tool_round_with_lifecycle(
                server_id,
                server_name,
                qualified_name,
                arguments,
                frozen_catalog_digest,
                context,
                None,
            )
            .await?
        {
            McpToolCallOutcome::Complete(result) => Ok(result),
            McpToolCallOutcome::InputRequired(_) => Err(McpFederationError::Protocol(
                "MCP Tool requires the continuation-aware execution path".into(),
            )),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the stdio Tool boundary keeps the frozen authority and lifecycle explicit"
    )]
    pub(crate) async fn call_tool_round_with_lifecycle(
        &self,
        server_id: Uuid,
        server_name: &str,
        qualified_name: &str,
        arguments: &Value,
        frozen_catalog_digest: &str,
        context: &McpCallContext,
        continuation: Option<&McpInputContinuation>,
    ) -> Result<McpToolCallOutcome, McpFederationError> {
        match self
            .request_with_lifecycle(
                server_id,
                SessionOperation::CallTool {
                    server_name: server_name.to_owned(),
                    qualified_name: qualified_name.to_owned(),
                    arguments: arguments.clone(),
                    frozen_catalog_digest: frozen_catalog_digest.to_owned(),
                    continuation: continuation.cloned(),
                },
                Some(context),
            )
            .await?
        {
            SessionResponse::Tool(result) => Ok(result),
            SessionResponse::Healthy
            | SessionResponse::Catalog(_)
            | SessionResponse::ResourcePage(_)
            | SessionResponse::ResourceRead(_)
            | SessionResponse::ResourceTemplatePage(_)
            | SessionResponse::PromptPage(_)
            | SessionResponse::Prompt(_) => Err(McpFederationError::Protocol(
                "stdio MCP returned a catalog to tools/call".into(),
            )),
        }
    }

    async fn request(
        &self,
        server_id: Uuid,
        operation: SessionOperation,
    ) -> Result<SessionResponse, McpFederationError> {
        self.request_with_lifecycle(server_id, operation, None)
            .await
    }

    async fn request_with_lifecycle(
        &self,
        server_id: Uuid,
        operation: SessionOperation,
        lifecycle: Option<&McpCallContext>,
    ) -> Result<SessionResponse, McpFederationError> {
        self.ensure_sweeper().await;
        let config = self
            .configs
            .get(&server_id)
            .cloned()
            .ok_or_else(|| McpFederationError::Protocol("unknown stdio MCP server".into()))?;
        for _ in 0..2 {
            let (sender, _lease) = self.sender(server_id, config.clone()).await?;
            let (response_tx, response_rx) = oneshot::channel();
            let cancellation = lifecycle
                .map(|context| context.cancellation.clone())
                .unwrap_or_default();
            let guard = CancelOnDrop::new(cancellation.clone());
            let request = SessionRequest {
                operation: clone_operation(&operation),
                cancellation,
                progress: lifecycle.map(|context| context.progress.clone()),
                progress_token: lifecycle.map(|context| context.progress_token.clone()),
                cancellation_reason: lifecycle
                    .map(|context| context.cancellation_reason.clone())
                    .unwrap_or_else(|| "MCP request was abandoned".into()),
                response: response_tx,
            };
            if sender.send(request).await.is_err() {
                guard.disarm();
                self.remove_if_same(server_id, &sender, false).await;
                continue;
            }
            match response_rx.await {
                Ok(result) => {
                    guard.disarm();
                    if result.is_err() {
                        // The actor closes after any protocol/transport error.
                        // Remove it before returning so a safe discovery retry
                        // cannot enqueue onto the dying session.
                        self.remove_if_same(server_id, &sender, true).await;
                    }
                    return result;
                }
                Err(_) => {
                    guard.disarm();
                    self.remove_if_same(server_id, &sender, true).await;
                    if matches!(operation, SessionOperation::CallTool { .. }) {
                        return Err(McpFederationError::Unreachable(
                            "stdio MCP session stopped after accepting the Tool request; its side-effect outcome is unknown"
                                .into(),
                        ));
                    }
                }
            }
        }
        Err(McpFederationError::Unreachable(
            "stdio MCP session stopped before accepting the request".into(),
        ))
    }

    async fn sender(
        &self,
        server_id: Uuid,
        config: LocalStdioMcpConfig,
    ) -> Result<(mpsc::Sender<SessionRequest>, SessionLease), McpFederationError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(handle) = sessions.get(&server_id)
            && !handle.sender.is_closed()
        {
            return Ok((handle.sender.clone(), handle.usage.acquire()));
        }
        let evicted = if sessions.len() >= self.max_sessions {
            let lru = sessions
                .iter()
                .filter(|(_, handle)| handle.usage.active() == 0)
                .max_by_key(|(_, handle)| handle.usage.idle_for())
                .map(|(server_id, _)| *server_id)
                .ok_or_else(|| {
                    McpFederationError::Unreachable(
                        "stdio MCP session capacity is fully leased".into(),
                    )
                })?;
            self.metrics.lru_evictions.fetch_add(1, Ordering::Relaxed);
            sessions.remove(&lru)
        } else {
            None
        };
        let (sender, receiver) = mpsc::channel(SESSION_QUEUE_CAPACITY);
        let timeout = self.request_timeout;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_session(config, receiver, timeout, shutdown.clone()));
        let usage = Arc::new(SessionUsage::new());
        let lease = usage.acquire();
        sessions.insert(
            server_id,
            SessionHandle {
                sender: sender.clone(),
                shutdown,
                task,
                usage,
            },
        );
        drop(sessions);
        if let Some(handle) = evicted {
            stop_sessions(vec![handle]).await;
        }
        Ok((sender, lease))
    }

    async fn ensure_sweeper(&self) {
        if self.session_idle_ttl.is_zero() || self.sweep_interval.is_zero() {
            return;
        }
        let mut sweeper = self.sweeper.lock().await;
        if sweeper.is_some() {
            return;
        }
        let sessions = Arc::clone(&self.sessions);
        let shutdown = self.lifetime.shutdown.clone();
        let metrics = Arc::clone(&self.metrics);
        let idle_ttl = self.session_idle_ttl;
        let sweep_interval = self.sweep_interval;
        *sweeper = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(sweep_interval) => {}
                }
                let handles = {
                    let mut sessions = sessions.lock().await;
                    let evict = sessions
                        .iter()
                        .filter_map(|(server_id, handle)| {
                            let closed = handle.sender.is_closed();
                            let idle =
                                handle.usage.active() == 0 && handle.usage.idle_for() >= idle_ttl;
                            (closed || idle).then_some((*server_id, idle))
                        })
                        .collect::<Vec<_>>();
                    evict
                        .into_iter()
                        .filter_map(|(server_id, idle)| {
                            let handle = sessions.remove(&server_id)?;
                            if idle {
                                metrics.idle_evictions.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(handle)
                        })
                        .collect::<Vec<_>>()
                };
                stop_sessions(handles).await;
            }
        }));
    }

    async fn remove_if_same(
        &self,
        server_id: Uuid,
        sender: &mpsc::Sender<SessionRequest>,
        failed: bool,
    ) {
        let handle = {
            let mut sessions = self.sessions.lock().await;
            if sessions
                .get(&server_id)
                .is_some_and(|current| current.sender.same_channel(sender))
            {
                sessions.remove(&server_id)
            } else {
                None
            }
        };
        if let Some(handle) = handle {
            if failed {
                self.metrics
                    .failed_session_retirements
                    .fetch_add(1, Ordering::Relaxed);
            }
            stop_sessions(vec![handle]).await;
        }
    }

    pub(crate) async fn lifecycle_snapshot(&self) -> LocalMcpLifecycleSnapshot {
        let sessions = self.sessions.lock().await;
        let active_leases = sessions.values().map(|handle| handle.usage.active()).sum();
        let live_sessions = sessions.len();
        drop(sessions);
        let cached_catalogs = self.catalogs.lock().await.len();
        LocalMcpLifecycleSnapshot {
            catalog_cache_hits: self.metrics.catalog_cache_hits.load(Ordering::Relaxed),
            catalog_cache_misses: self.metrics.catalog_cache_misses.load(Ordering::Relaxed),
            failed_session_retirements: self
                .metrics
                .failed_session_retirements
                .load(Ordering::Relaxed),
            live_sessions,
            active_leases,
            cached_catalogs,
            idle_evictions: self.metrics.idle_evictions.load(Ordering::Relaxed),
            lru_evictions: self.metrics.lru_evictions.load(Ordering::Relaxed),
        }
    }

    /// Stops every persistent stdio session and waits until its process group
    /// has been reaped. The explicit await is essential at binary shutdown:
    /// dropping a Tokio task only detaches it, while dropping the runtime can
    /// abort it before asynchronous process-tree cleanup runs.
    pub(crate) async fn shutdown(&self) {
        self.lifetime.shutdown.cancel();
        if let Some(sweeper) = self.sweeper.lock().await.take() {
            let _ = sweeper.await;
        }
        let handles = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>()
        };
        stop_sessions(handles).await;
    }
}

async fn stop_sessions(handles: Vec<SessionHandle>) {
    for handle in &handles {
        handle.shutdown.cancel();
    }
    for handle in handles {
        drop(handle.sender);
        let _ = handle.task.await;
    }
}

fn clone_operation(operation: &SessionOperation) -> SessionOperation {
    match operation {
        SessionOperation::Health => SessionOperation::Health,
        SessionOperation::ListTools { server_name } => SessionOperation::ListTools {
            server_name: server_name.clone(),
        },
        SessionOperation::ListResources {
            server_name,
            frozen_catalog_digest,
            cursor,
        } => SessionOperation::ListResources {
            server_name: server_name.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            cursor: cursor.clone(),
        },
        SessionOperation::ReadResource {
            server_name,
            frozen_catalog_digest,
            uri,
        } => SessionOperation::ReadResource {
            server_name: server_name.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            uri: uri.clone(),
        },
        SessionOperation::ListResourceTemplates {
            server_name,
            frozen_catalog_digest,
            cursor,
        } => SessionOperation::ListResourceTemplates {
            server_name: server_name.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            cursor: cursor.clone(),
        },
        SessionOperation::ListPrompts {
            server_name,
            frozen_catalog_digest,
            cursor,
        } => SessionOperation::ListPrompts {
            server_name: server_name.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            cursor: cursor.clone(),
        },
        SessionOperation::GetPrompt {
            server_name,
            frozen_catalog_digest,
            name,
            arguments,
        } => SessionOperation::GetPrompt {
            server_name: server_name.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
        },
        SessionOperation::CallTool {
            server_name,
            qualified_name,
            arguments,
            frozen_catalog_digest,
            continuation,
        } => SessionOperation::CallTool {
            server_name: server_name.clone(),
            qualified_name: qualified_name.clone(),
            arguments: arguments.clone(),
            frozen_catalog_digest: frozen_catalog_digest.clone(),
            continuation: continuation.clone(),
        },
    }
}

async fn run_session(
    config: LocalStdioMcpConfig,
    mut receiver: mpsc::Receiver<SessionRequest>,
    request_timeout: Duration,
    shutdown: CancellationToken,
) {
    let mut process = match StdioProcess::spawn(&config) {
        Ok(process) => process,
        Err(error) => {
            loop {
                let request = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    request = receiver.recv() => request,
                };
                let Some(request) = request else {
                    break;
                };
                let _ = request
                    .response
                    .send(Err(McpFederationError::Unreachable(error.to_string())));
            }
            return;
        }
    };

    let mut initialized = false;
    loop {
        let request = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            request = receiver.recv() => request,
        };
        let Some(request) = request else {
            break;
        };
        match process.has_exited() {
            Ok(true) => {
                let _ = request.response.send(Err(McpFederationError::Unreachable(
                    "stdio MCP server process exited".into(),
                )));
                break;
            }
            Ok(false) => {}
            Err(error) => {
                let _ = request.response.send(Err(error));
                break;
            }
        }
        if !initialized {
            let initialization = tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(McpFederationError::Unreachable(
                    "stdio MCP session was shut down".into()
                )),
                _ = request.cancellation.cancelled() => Err(McpFederationError::Unreachable(
                    "stdio MCP initialize was cancelled".into()
                )),
                result = process.initialize(request_timeout) => result,
            };
            if let Err(error) = initialization {
                let _ = request.response.send(Err(error));
                break;
            }
            initialized = true;
        }
        let tool_call_owns_protocol_cancellation =
            matches!(&request.operation, SessionOperation::CallTool { .. });
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => Err(McpFederationError::Unreachable(
                "stdio MCP session was shut down".into()
            )),
            _ = request.cancellation.cancelled(), if !tool_call_owns_protocol_cancellation => {
                Err(McpFederationError::Unreachable(
                    "stdio MCP request was cancelled".into()
                ))
            },
            result = tokio::time::timeout(
                request_timeout,
                process.apply(
                    request.operation,
                    &request.cancellation,
                    request.progress.as_ref(),
                    request.progress_token.as_deref(),
                    &request.cancellation_reason,
                )
            ) => match result {
                Ok(result) => result,
                Err(_) => Err(McpFederationError::Unreachable(
                    "stdio MCP request timed out".into()
                )),
            },
        };
        let must_close = result.is_err() || request.cancellation.is_cancelled();
        let _ = request.response.send(result);
        if must_close {
            break;
        }
    }
    process.terminate().await;
}

struct StdioProcess {
    child: Child,
    #[cfg(unix)]
    process_group_id: libc::pid_t,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pending_line: Vec<u8>,
    next_id: u64,
    protocol_revision: McpProtocolRevision,
    client_capabilities: std::collections::BTreeSet<agent_protocol::McpClientCapability>,
    server_capabilities: std::collections::BTreeSet<McpServerCapability>,
}

impl StdioProcess {
    fn spawn(config: &LocalStdioMcpConfig) -> Result<Self, std::io::Error> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .env_clear();
        for name in default_environment_names() {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.envs(&config.env);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        #[cfg(unix)]
        let process_group_id = child
            .id()
            .map(|pid| pid as libc::pid_t)
            .ok_or_else(|| std::io::Error::other("stdio MCP process has no pid after spawn"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("stdio MCP stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("stdio MCP stdout was not piped"))?;
        Ok(Self {
            child,
            #[cfg(unix)]
            process_group_id,
            stdin,
            stdout: BufReader::new(stdout),
            pending_line: Vec::new(),
            next_id: 1,
            protocol_revision: config.protocol_revision,
            client_capabilities: config.client_capabilities.clone(),
            server_capabilities: Default::default(),
        })
    }

    fn has_exited(&mut self) -> Result<bool, McpFederationError> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))
    }

    async fn initialize(&mut self, timeout: Duration) -> Result<(), McpFederationError> {
        if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
            let mut params = serde_json::Map::new();
            attach_modern_request_metadata(&self.client_capabilities, &mut params)?;
            let discovery =
                tokio::time::timeout(timeout, self.rpc("server/discover", Value::Object(params)))
                    .await
                    .map_err(|_| {
                        McpFederationError::Unreachable(
                            "stdio MCP server/discover timed out".into(),
                        )
                    })??;
            self.server_capabilities = validate_modern_discovery_result(&discovery)?;
            return Ok(());
        }
        let initialize_result = tokio::time::timeout(
            timeout,
            self.rpc(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "agent-runtime-platform", "version": "1"}
                }),
            ),
        )
        .await
        .map_err(|_| McpFederationError::Unreachable("stdio MCP initialize timed out".into()))??;
        self.server_capabilities = validate_initialize_result(&initialize_result)?;
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await
    }

    async fn verify_read_surface(
        &mut self,
        server_name: &str,
        frozen_catalog_digest: &str,
        capability: McpServerCapability,
    ) -> Result<(), McpFederationError> {
        if !self.server_capabilities.contains(&capability) {
            return Err(McpFederationError::CatalogChanged);
        }
        let catalog = if self
            .server_capabilities
            .contains(&McpServerCapability::Tools)
        {
            let params = self.request_params(serde_json::json!({}))?;
            let listed = self.rpc("tools/list", params).await?;
            if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                validate_modern_complete_result(&listed, "tools/list")?;
            }
            catalog_from_list_result(server_name, &listed, self.server_capabilities.clone())?
        } else {
            empty_catalog_for_capabilities(self.server_capabilities.clone())
        };
        if catalog.digest != frozen_catalog_digest {
            return Err(McpFederationError::CatalogChanged);
        }
        Ok(())
    }

    async fn apply(
        &mut self,
        operation: SessionOperation,
        cancellation: &CancellationToken,
        progress: Option<&mpsc::Sender<McpProgressNotification>>,
        progress_token: Option<&str>,
        cancellation_reason: &str,
    ) -> Result<SessionResponse, McpFederationError> {
        match operation {
            SessionOperation::Health => {
                let params = self.request_params(serde_json::json!({}))?;
                let result = self.rpc("ping", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "ping")?;
                } else if !result.is_object() {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP ping result is not an object".into(),
                    ));
                }
                Ok(SessionResponse::Healthy)
            }
            SessionOperation::ListTools { server_name } => {
                if !self
                    .server_capabilities
                    .contains(&McpServerCapability::Tools)
                {
                    return Ok(SessionResponse::Catalog(empty_catalog_for_capabilities(
                        self.server_capabilities.clone(),
                    )));
                }
                let params = self.request_params(serde_json::json!({}))?;
                let result = self.rpc("tools/list", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "tools/list")?;
                }
                Ok(SessionResponse::Catalog(catalog_from_list_result(
                    &server_name,
                    &result,
                    self.server_capabilities.clone(),
                )?))
            }
            SessionOperation::ListResources {
                server_name,
                frozen_catalog_digest,
                cursor,
            } => {
                if cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2 * 1024)
                {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP pagination cursor is empty or unbounded".into(),
                    ));
                }
                self.verify_read_surface(
                    &server_name,
                    &frozen_catalog_digest,
                    McpServerCapability::Resources,
                )
                .await?;
                let mut params = serde_json::Map::new();
                if let Some(cursor) = cursor {
                    params.insert("cursor".into(), Value::String(cursor));
                }
                let params = self.request_params(Value::Object(params))?;
                let result = self.rpc("resources/list", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "resources/list")?;
                }
                Ok(SessionResponse::ResourcePage(
                    resource_page_from_list_result(&result)?,
                ))
            }
            SessionOperation::ReadResource {
                server_name,
                frozen_catalog_digest,
                uri,
            } => {
                if uri.is_empty() || uri.len() > 4 * 1024 || uri.chars().any(char::is_control) {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP resource URI is empty or unbounded".into(),
                    ));
                }
                self.verify_read_surface(
                    &server_name,
                    &frozen_catalog_digest,
                    McpServerCapability::Resources,
                )
                .await?;
                let params = self.request_params(serde_json::json!({"uri": uri}))?;
                let result = self.rpc("resources/read", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "resources/read")?;
                }
                Ok(SessionResponse::ResourceRead(resource_read_from_result(
                    &result,
                )?))
            }
            SessionOperation::ListResourceTemplates {
                server_name,
                frozen_catalog_digest,
                cursor,
            } => {
                if cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2 * 1024)
                {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP pagination cursor is empty or unbounded".into(),
                    ));
                }
                self.verify_read_surface(
                    &server_name,
                    &frozen_catalog_digest,
                    McpServerCapability::Resources,
                )
                .await?;
                let mut params = serde_json::Map::new();
                if let Some(cursor) = cursor {
                    params.insert("cursor".into(), Value::String(cursor));
                }
                let params = self.request_params(Value::Object(params))?;
                let result = self.rpc("resources/templates/list", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "resources/templates/list")?;
                }
                Ok(SessionResponse::ResourceTemplatePage(
                    resource_template_page_from_list_result(&result)?,
                ))
            }
            SessionOperation::ListPrompts {
                server_name,
                frozen_catalog_digest,
                cursor,
            } => {
                if cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2 * 1024)
                {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP pagination cursor is empty or unbounded".into(),
                    ));
                }
                self.verify_read_surface(
                    &server_name,
                    &frozen_catalog_digest,
                    McpServerCapability::Prompts,
                )
                .await?;
                let mut params = serde_json::Map::new();
                if let Some(cursor) = cursor {
                    params.insert("cursor".into(), Value::String(cursor));
                }
                let params = self.request_params(Value::Object(params))?;
                let result = self.rpc("prompts/list", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "prompts/list")?;
                }
                Ok(SessionResponse::PromptPage(prompt_page_from_list_result(
                    &result,
                )?))
            }
            SessionOperation::GetPrompt {
                server_name,
                frozen_catalog_digest,
                name,
                arguments,
            } => {
                if name.is_empty() || name.len() > 128 {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP prompt name is empty or unbounded".into(),
                    ));
                }
                if arguments.as_ref().is_some_and(|arguments| {
                    arguments.as_object().is_none_or(|object| {
                        object.len() > 32 || object.values().any(|value| !value.is_string())
                    }) || serde_json::to_vec(arguments)
                        .map_or(true, |encoded| encoded.len() > 64 * 1024)
                }) {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP prompt arguments are malformed or unbounded".into(),
                    ));
                }
                self.verify_read_surface(
                    &server_name,
                    &frozen_catalog_digest,
                    McpServerCapability::Prompts,
                )
                .await?;
                let mut params = serde_json::Map::from_iter([("name".into(), Value::String(name))]);
                if let Some(arguments) = arguments {
                    params.insert("arguments".into(), arguments);
                }
                let params = self.request_params(Value::Object(params))?;
                let result = self.rpc("prompts/get", params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&result, "prompts/get")?;
                }
                Ok(SessionResponse::Prompt(prompt_result_from_get_result(
                    &result,
                )?))
            }
            SessionOperation::CallTool {
                server_name,
                qualified_name,
                arguments,
                frozen_catalog_digest,
                continuation,
            } => {
                if !self
                    .server_capabilities
                    .contains(&McpServerCapability::Tools)
                {
                    return Err(McpFederationError::ToolNotInFrozenCatalog(qualified_name));
                }
                if self.protocol_revision == McpProtocolRevision::V2025_06_18
                    && continuation.is_some()
                {
                    return Err(McpFederationError::Protocol(
                        "MCP 2025-06-18 does not support stateless MRTR continuation".into(),
                    ));
                }
                let list_params = self.request_params(serde_json::json!({}))?;
                let listed = self.rpc("tools/list", list_params).await?;
                if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    validate_modern_complete_result(&listed, "tools/list")?;
                }
                let catalog = catalog_from_list_result(
                    &server_name,
                    &listed,
                    self.server_capabilities.clone(),
                )?;
                if catalog.digest != frozen_catalog_digest {
                    return Err(McpFederationError::CatalogChanged);
                }
                if !catalog
                    .tools
                    .iter()
                    .any(|tool| tool.qualified_name == qualified_name)
                {
                    return Err(McpFederationError::ToolNotInFrozenCatalog(qualified_name));
                }
                let bare = qualified_name
                    .rsplit_once('/')
                    .map(|(_, tool)| tool)
                    .ok_or_else(|| {
                        McpFederationError::ToolNotInFrozenCatalog(qualified_name.clone())
                    })?;
                let progress_token = progress_token.ok_or_else(|| {
                    McpFederationError::Protocol("stdio MCP progress token is missing".into())
                })?;
                let round = continuation.as_ref().map_or(1, |value| value.round);
                if !(1..=10).contains(&round) {
                    return Err(McpFederationError::Protocol(
                        "MCP MRTR round must be between 1 and 10".into(),
                    ));
                }
                let mut params = serde_json::json!({
                    "name": bare,
                    "arguments": arguments,
                    "_meta": {"progressToken": progress_token}
                });
                if let Some(continuation) = continuation {
                    if continuation.request_state.is_empty()
                        || continuation.request_state.len() > 64 * 1024
                        || continuation.responses.is_empty()
                        || continuation.responses.len() > 8
                    {
                        return Err(McpFederationError::Protocol(
                            "MCP MRTR continuation is malformed or unbounded".into(),
                        ));
                    }
                    params["requestState"] = Value::String(continuation.request_state);
                    params["inputResponses"] = mrtr_responses_value(&continuation.responses)?;
                }
                let params = self.request_params(params)?;
                let result = self
                    .rpc_tool(
                        params,
                        cancellation,
                        progress,
                        progress_token,
                        cancellation_reason,
                    )
                    .await?;
                let outcome = if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
                    match result.get("resultType").and_then(Value::as_str) {
                        Some("complete") => {
                            McpToolCallOutcome::Complete(tool_result_from_call_result(&result))
                        }
                        Some("input_required") => {
                            if round == 10 {
                                return Err(McpFederationError::Protocol(
                                    "MCP Tool exceeded the 10-round MRTR limit".into(),
                                ));
                            }
                            McpToolCallOutcome::InputRequired(parse_modern_input_required(
                                &self.client_capabilities,
                                &result,
                                round,
                            )?)
                        }
                        other => {
                            return Err(McpFederationError::Protocol(format!(
                                "tools/call returned unsupported resultType {other:?}"
                            )));
                        }
                    }
                } else {
                    McpToolCallOutcome::Complete(tool_result_from_call_result(&result))
                };
                Ok(SessionResponse::Tool(outcome))
            }
        }
    }

    fn request_params(&self, mut params: Value) -> Result<Value, McpFederationError> {
        if self.protocol_revision == McpProtocolRevision::V2026_07_28 {
            let object = params.as_object_mut().ok_or_else(|| {
                McpFederationError::Protocol("modern MCP params must be an object".into())
            })?;
            attach_modern_request_metadata(&self.client_capabilities, object)?;
        }
        Ok(params)
    }

    async fn rpc_tool(
        &mut self,
        params: Value,
        cancellation: &CancellationToken,
        progress_sender: Option<&mpsc::Sender<McpProgressNotification>>,
        progress_token: &str,
        cancellation_reason: &str,
    ) -> Result<Value, McpFederationError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        }))
        .await?;
        let mut last_progress = None;
        loop {
            let message = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    self.write_message(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": {"requestId": id, "reason": cancellation_reason}
                    })).await?;
                    // The notification has no response. Give the stdio peer a
                    // short cooperative window to consume it and exit before
                    // process-group reaping enforces the hard boundary.
                    let _ = tokio::time::timeout(
                        Duration::from_millis(100),
                        self.read_message(),
                    ).await;
                    return Err(McpFederationError::Cancelled);
                }
                message = self.read_message() => message?,
            };
            if message["method"] == "notifications/progress" {
                let params = &message["params"];
                if params["progressToken"] != progress_token {
                    continue;
                }
                let value = params["progress"].as_f64().ok_or_else(|| {
                    McpFederationError::Protocol("stdio MCP progress must be numeric".into())
                })?;
                if !value.is_finite() || last_progress.is_some_and(|last| value <= last) {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP progress must increase monotonically".into(),
                    ));
                }
                let total = params["total"].as_f64();
                if total.is_some_and(|total| !total.is_finite()) {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP progress total must be finite".into(),
                    ));
                }
                let message = params["message"].as_str().map(str::to_owned);
                if message
                    .as_ref()
                    .is_some_and(|message| message.len() > 2_048)
                {
                    return Err(McpFederationError::Protocol(
                        "stdio MCP progress message exceeded 2048 bytes".into(),
                    ));
                }
                last_progress = Some(value);
                if let Some(sender) = progress_sender {
                    let _ = sender.try_send(McpProgressNotification {
                        progress: value,
                        total,
                        message,
                    });
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown JSON-RPC error");
                    return Err(McpFederationError::Protocol(detail.to_owned()));
                }
                return message.get("result").cloned().ok_or_else(|| {
                    McpFederationError::Protocol("stdio MCP response has no result".into())
                });
            }
            if message.get("method").is_some()
                && let Some(server_id) = message.get("id").cloned()
            {
                let method = message["method"].as_str().unwrap_or("unknown").to_owned();
                self.write_message(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "error": {"code": -32601, "message": "client method is not supported"}
                }))
                .await?;
                // Give the peer a bounded window to consume the rejection
                // before session retirement reaps its process group. Any
                // subsequent result is deliberately discarded: the protocol
                // violation has already made the Tool outcome untrustworthy.
                let _ = tokio::time::timeout(Duration::from_millis(100), self.read_message()).await;
                return Err(McpFederationError::Protocol(format!(
                    "stdio MCP server sent unnegotiated client request {method}"
                )));
            }
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, McpFederationError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        loop {
            let message = self.read_message().await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown JSON-RPC error");
                    return Err(McpFederationError::Protocol(detail.to_owned()));
                }
                return message.get("result").cloned().ok_or_else(|| {
                    McpFederationError::Protocol("stdio MCP response has no result".into())
                });
            }
            if message.get("method").is_some()
                && let Some(server_id) = message.get("id").cloned()
            {
                let method = message["method"].as_str().unwrap_or("unknown").to_owned();
                self.write_message(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "error": {"code": -32601, "message": "client method is not supported"}
                }))
                .await?;
                let _ = tokio::time::timeout(Duration::from_millis(100), self.read_message()).await;
                return Err(McpFederationError::Protocol(format!(
                    "stdio MCP server sent unnegotiated client request {method}"
                )));
            }
        }
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), McpFederationError> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|error| McpFederationError::Protocol(error.to_string()))?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| McpFederationError::Unreachable(error.to_string()))
    }

    async fn read_message(&mut self) -> Result<Value, McpFederationError> {
        loop {
            let bytes = self
                .stdout
                .fill_buf()
                .await
                .map_err(|error| McpFederationError::Unreachable(error.to_string()))?;
            if bytes.is_empty() {
                return Err(McpFederationError::Unreachable(
                    "stdio MCP server closed stdout".into(),
                ));
            }
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(bytes.len());
            if content_len > MAX_STDIO_MESSAGE_BYTES.saturating_sub(self.pending_line.len()) {
                return Err(McpFederationError::ResponseTooLarge);
            }
            self.pending_line.extend_from_slice(&bytes[..content_len]);
            self.stdout
                .consume(content_len + usize::from(newline.is_some()));
            if newline.is_some() {
                let line = std::mem::take(&mut self.pending_line);
                let line = line.strip_suffix(b"\r").unwrap_or(&line);
                return serde_json::from_slice(line)
                    .map_err(|error| McpFederationError::Protocol(error.to_string()));
            }
        }
    }

    async fn terminate(&mut self) {
        #[cfg(unix)]
        {
            // `process_group(0)` makes the child's initial PID its PGID. Keep
            // that value from spawn: querying by PID here loses the group when
            // the leader exits before a descendant that ignores TERM.
            let pgid = self.process_group_id;
            if pgid > 0 && pgid != unsafe { libc::getpgrp() } {
                unsafe {
                    libc::killpg(pgid, libc::SIGTERM);
                }
                let direct_child_reaped =
                    tokio::time::timeout(Duration::from_millis(250), self.child.wait())
                        .await
                        .is_ok();
                // The direct shell may exit while a descendant ignores TERM.
                // Process-group existence, not direct-child exit, decides
                // whether cleanup is complete.
                if unsafe { libc::killpg(pgid, 0) } == 0 {
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                }
                if direct_child_reaped {
                    return;
                }
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.child.id() {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

fn validate_initialize_result(
    result: &Value,
) -> Result<std::collections::BTreeSet<McpServerCapability>, McpFederationError> {
    let selected = result["protocolVersion"].as_str().ok_or_else(|| {
        McpFederationError::Protocol("stdio MCP initialize result has no protocolVersion".into())
    })?;
    if selected != MCP_PROTOCOL_VERSION {
        return Err(McpFederationError::Protocol(format!(
            "stdio MCP server selected unsupported protocol version {selected}"
        )));
    }
    let capabilities = parse_server_capabilities(&result["capabilities"])?;
    if capabilities.is_empty() {
        return Err(McpFederationError::Protocol(
            "stdio MCP server did not negotiate a supported capability".into(),
        ));
    }
    Ok(capabilities)
}

fn validate_modern_discovery_result(
    result: &Value,
) -> Result<std::collections::BTreeSet<McpServerCapability>, McpFederationError> {
    validate_modern_complete_result(result, "server/discover")?;
    if !result["supportedVersions"]
        .as_array()
        .is_some_and(|versions| {
            versions
                .iter()
                .any(|version| version.as_str() == Some(MODERN_MCP_PROTOCOL_VERSION))
        })
    {
        return Err(McpFederationError::Protocol(
            "stdio MCP server/discover did not advertise 2026-07-28".into(),
        ));
    }
    let capabilities = parse_server_capabilities(&result["capabilities"])?;
    if capabilities.is_empty() {
        return Err(McpFederationError::Protocol(
            "stdio MCP server did not advertise a supported capability".into(),
        ));
    }
    Ok(capabilities)
}

fn parse_server_capabilities(
    value: &Value,
) -> Result<std::collections::BTreeSet<McpServerCapability>, McpFederationError> {
    let object = value.as_object().ok_or_else(|| {
        McpFederationError::Protocol("stdio MCP server capabilities must be an object".into())
    })?;
    let mut capabilities = std::collections::BTreeSet::new();
    for (field, capability) in [
        ("tools", McpServerCapability::Tools),
        ("resources", McpServerCapability::Resources),
        ("prompts", McpServerCapability::Prompts),
    ] {
        if let Some(value) = object.get(field) {
            if !value.is_object() {
                return Err(McpFederationError::Protocol(format!(
                    "stdio MCP server capability {field} must be an object"
                )));
            }
            capabilities.insert(capability);
        }
    }
    Ok(capabilities)
}

fn validate_modern_complete_result(result: &Value, method: &str) -> Result<(), McpFederationError> {
    if result.get("resultType").and_then(Value::as_str) != Some("complete") {
        return Err(McpFederationError::Protocol(format!(
            "stdio MCP {method} did not return a complete result"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn default_environment_names() -> &'static [&'static str] {
    &[
        "HOME",
        "LOGNAME",
        "PATH",
        "SHELL",
        "USER",
        "__CF_USER_TEXT_ENCODING",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TZ",
    ]
}

#[cfg(windows)]
fn default_environment_names() -> &'static [&'static str] {
    &[
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "COMSPEC",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ]
}

#[cfg(not(any(unix, windows)))]
fn default_environment_names() -> &'static [&'static str] {
    &[]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn read_pids(path: &Path) -> Vec<libc::pid_t> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }

    async fn process_exited(pid: libc::pid_t) -> bool {
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Resources and Prompts are server surfaces, not implicit Tool authority.
    /// A directory-only stdio server must initialize successfully without ever
    /// receiving `tools/list` or becoming able to execute a Tool.
    #[tokio::test]
    async fn resource_and_prompt_only_stdio_server_skips_tool_discovery() {
        let state = tempfile::tempdir().expect("fixture state");
        let lists = state.path().join("lists");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c5);
        let client = StdioMcpClient::new(
            HashMap::from([(
                server_id,
                LocalStdioMcpConfig {
                    command: Path::new("/bin/sh").to_path_buf(),
                    args: vec![script.to_string_lossy().into_owned()],
                    env: BTreeMap::from([
                        (
                            "MCP_SERVER_CAPABILITIES_JSON".into(),
                            r#"{"resources":{"listChanged":true},"prompts":{}}"#.into(),
                        ),
                        (
                            "MCP_LIST_MARKER".into(),
                            lists.to_string_lossy().into_owned(),
                        ),
                    ]),
                    cwd: None,
                    protocol_revision: McpProtocolRevision::V2025_06_18,
                    client_capabilities: Default::default(),
                },
            )]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig::default(),
        );

        let catalog = client
            .list_tools(server_id, "knowledge")
            .await
            .expect("directory-only server must initialize");
        client.shutdown().await;

        assert!(catalog.tools.is_empty());
        assert_eq!(
            catalog.capabilities,
            std::collections::BTreeSet::from([
                McpServerCapability::Resources,
                McpServerCapability::Prompts,
            ])
        );
        assert_eq!(catalog.digest.len(), 64);
        assert!(
            !lists.exists(),
            "a server without Tool capability received tools/list"
        );
    }

    #[tokio::test]
    async fn stdio_resources_and_prompts_use_the_same_bounded_types_as_http() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c6);
        let client = StdioMcpClient::new(
            HashMap::from([(
                server_id,
                LocalStdioMcpConfig {
                    command: Path::new("/bin/sh").to_path_buf(),
                    args: vec![script.to_string_lossy().into_owned()],
                    env: BTreeMap::from([(
                        "MCP_SERVER_CAPABILITIES_JSON".into(),
                        r#"{"resources":{},"prompts":{}}"#.into(),
                    )]),
                    cwd: None,
                    protocol_revision: McpProtocolRevision::V2025_06_18,
                    client_capabilities: Default::default(),
                },
            )]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig::default(),
        );
        let catalog = client.list_tools(server_id, "knowledge").await.unwrap();

        let resources = client
            .list_resources(
                server_id,
                "knowledge",
                &catalog.digest,
                Some("resource-page-1"),
            )
            .await
            .unwrap();
        assert_eq!(resources.resources[0].uri, "kb://local/runbook");
        assert_eq!(resources.next_cursor.as_deref(), Some("resource-page-2"));

        let read = client
            .read_resource(
                server_id,
                "knowledge",
                &catalog.digest,
                "kb://local/runbook",
            )
            .await
            .unwrap();
        assert_eq!(read.contents.len(), 2);

        let templates = client
            .list_resource_templates(
                server_id,
                "knowledge",
                &catalog.digest,
                Some("template-page-1"),
            )
            .await
            .unwrap();
        assert_eq!(templates.resource_templates[0].name, "knowledge");
        assert_eq!(templates.next_cursor.as_deref(), Some("template-page-2"));

        let prompts = client
            .list_prompts(
                server_id,
                "knowledge",
                &catalog.digest,
                Some("prompt-page-1"),
            )
            .await
            .unwrap();
        assert_eq!(prompts.prompts[0].name, "summarize");

        let prompt = client
            .get_prompt(
                server_id,
                "knowledge",
                &catalog.digest,
                "summarize",
                Some(&serde_json::json!({"tone": "short"})),
            )
            .await
            .unwrap();
        assert_eq!(prompt.description.as_deref(), Some("resolved"));
        assert_eq!(prompt.messages[0].role, "user");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn modern_stdio_resources_and_prompts_share_the_same_contract() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_2026_server.sh")
            .canonicalize()
            .expect("modern fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c7);
        let client = StdioMcpClient::new(
            HashMap::from([(
                server_id,
                LocalStdioMcpConfig {
                    command: Path::new("/bin/sh").to_path_buf(),
                    args: vec![script.to_string_lossy().into_owned()],
                    env: BTreeMap::from([("MCP_READ_SURFACES".into(), "1".into())]),
                    cwd: None,
                    protocol_revision: McpProtocolRevision::V2026_07_28,
                    client_capabilities: std::collections::BTreeSet::from([
                        agent_protocol::McpClientCapability::Elicitation,
                    ]),
                },
            )]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig::default(),
        );
        let catalog = client.list_tools(server_id, "knowledge").await.unwrap();
        assert!(catalog.tools.is_empty());
        assert_eq!(
            client
                .list_resources(server_id, "knowledge", &catalog.digest, None)
                .await
                .unwrap()
                .resources[0]
                .uri,
            "kb://modern-stdio/runbook"
        );
        assert_eq!(
            client
                .read_resource(
                    server_id,
                    "knowledge",
                    &catalog.digest,
                    "kb://modern-stdio/runbook"
                )
                .await
                .unwrap()
                .contents
                .len(),
            1
        );
        assert_eq!(
            client
                .list_resource_templates(server_id, "knowledge", &catalog.digest, None)
                .await
                .unwrap()
                .resource_templates[0]
                .uri_template,
            "kb://modern-stdio/{name}"
        );
        assert_eq!(
            client
                .list_prompts(server_id, "knowledge", &catalog.digest, None)
                .await
                .unwrap()
                .prompts[0]
                .name,
            "summarize"
        );
        assert_eq!(
            client
                .get_prompt(server_id, "knowledge", &catalog.digest, "summarize", None)
                .await
                .unwrap()
                .messages[0]
                .role,
            "user"
        );
        client.shutdown().await;
    }

    /// A cached directory is an optimization, not proof that its authority is
    /// alive. A dead session must fail once, be retired, then a fresh initialized
    /// session may reuse the still-fresh directory without another tools/list.
    #[tokio::test]
    async fn closed_session_reconnects_before_reusing_healthy_catalog() {
        let state = tempfile::tempdir().expect("fixture state");
        let starts = state.path().join("starts");
        let lists = state.path().join("lists");
        let pids = state.path().join("grandchildren");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c1);
        let config = LocalStdioMcpConfig {
            command: Path::new("/bin/sh").to_path_buf(),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([
                (
                    "MCP_START_MARKER".into(),
                    starts.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_LIST_MARKER".into(),
                    lists.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_GRANDCHILD_PID_LOG".into(),
                    pids.to_string_lossy().into_owned(),
                ),
                ("MCP_EXIT_AFTER_LIST_ATTEMPTS".into(), "1".into()),
            ]),
            cwd: None,
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: Default::default(),
        };
        let client = StdioMcpClient::new(
            HashMap::from([(server_id, config)]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig {
                catalog_ttl: Duration::from_secs(30),
                ..LocalMcpLifecycleConfig::default()
            },
        );

        let first = client.list_tools(server_id, "local").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let closed = client.list_tools(server_id, "local").await;
        let recovered = client.list_tools(server_id, "local").await;
        let snapshot = client.lifecycle_snapshot().await;
        let start_count = std::fs::read_to_string(&starts).unwrap_or_default();
        let list_count = std::fs::read_to_string(&lists).unwrap_or_default();
        client.shutdown().await;
        let child_pids = read_pids(&pids);

        assert!(first.is_ok(), "initial directory failed: {first:?}");
        assert!(
            matches!(closed, Err(McpFederationError::Unreachable(_))),
            "closed authority was trusted: {closed:?}"
        );
        assert!(recovered.is_ok(), "fresh session failed: {recovered:?}");
        assert_eq!(start_count, "started\nstarted\n");
        assert_eq!(
            list_count, "listed\n",
            "session replacement must not discard a fresh directory cache"
        );
        assert_eq!(snapshot.catalog_cache_hits, 1);
        assert_eq!(snapshot.catalog_cache_misses, 1);
        assert_eq!(snapshot.failed_session_retirements, 1);
        for pid in child_pids {
            assert!(process_exited(pid).await, "session child {pid} survived");
        }
    }

    /// A live PID is not sufficient proof that the MCP protocol is responsive.
    /// Cached catalogs may only be reused after the server answers MCP ping.
    #[tokio::test]
    async fn unresponsive_live_session_cannot_authorize_cached_catalog_reuse() {
        let state = tempfile::tempdir().expect("fixture state");
        let starts = state.path().join("starts");
        let pings = state.path().join("pings");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c4);
        let config = LocalStdioMcpConfig {
            command: Path::new("/bin/sh").to_path_buf(),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([
                (
                    "MCP_START_MARKER".into(),
                    starts.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_PING_MARKER".into(),
                    pings.to_string_lossy().into_owned(),
                ),
                ("MCP_STALL_PING".into(), "1".into()),
            ]),
            cwd: None,
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: Default::default(),
        };
        let client = StdioMcpClient::new(
            HashMap::from([(server_id, config)]),
            Duration::from_millis(200),
            LocalMcpLifecycleConfig {
                catalog_ttl: Duration::from_secs(30),
                ..LocalMcpLifecycleConfig::default()
            },
        );

        let first = client.list_tools(server_id, "local").await;
        let cached = client.list_tools(server_id, "local").await;
        let snapshot = client.lifecycle_snapshot().await;
        client.shutdown().await;

        assert!(first.is_ok(), "initial directory failed: {first:?}");
        assert!(
            matches!(cached, Err(McpFederationError::Unreachable(_))),
            "unresponsive authority was trusted: {cached:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&pings).unwrap_or_default(),
            "ping\n"
        );
        assert_eq!(snapshot.catalog_cache_hits, 0);
        assert_eq!(snapshot.catalog_cache_misses, 1);
        assert_eq!(snapshot.failed_session_retirements, 1);
        assert_eq!(
            std::fs::read_to_string(&starts).unwrap_or_default(),
            "started\n"
        );
    }

    /// Idle collection must never turn a slow Tool call into an ambiguous
    /// failure. The active lease protects the session until the response is
    /// known; only then may the bounded idle sweeper reap its process group.
    #[tokio::test]
    async fn active_tool_call_is_leased_then_idle_session_is_reaped() {
        let state = tempfile::tempdir().expect("fixture state");
        let starts = state.path().join("starts");
        let lists = state.path().join("lists");
        let calls = state.path().join("calls");
        let pids = state.path().join("grandchildren");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c2);
        let config = LocalStdioMcpConfig {
            command: Path::new("/bin/sh").to_path_buf(),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([
                (
                    "MCP_START_MARKER".into(),
                    starts.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_LIST_MARKER".into(),
                    lists.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_CALL_MARKER".into(),
                    calls.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_GRANDCHILD_PID_LOG".into(),
                    pids.to_string_lossy().into_owned(),
                ),
                ("MCP_CALL_DELAY_SECONDS".into(), "1".into()),
            ]),
            cwd: None,
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: Default::default(),
        };
        let client = StdioMcpClient::new(
            HashMap::from([(server_id, config)]),
            Duration::from_secs(2),
            crate::LocalMcpLifecycleConfig {
                catalog_ttl: Duration::from_secs(30),
                session_idle_ttl: Duration::from_millis(50),
                sweep_interval: Duration::from_millis(10),
                ..LocalMcpLifecycleConfig::default()
            },
        );
        let catalog = client
            .list_tools(server_id, "local")
            .await
            .expect("initial directory");
        let digest = catalog.digest.clone();
        let qualified_name = catalog.tools[0].qualified_name.clone();
        let call_client = client.clone();
        let call = tokio::spawn(async move {
            call_client
                .call_tool(
                    server_id,
                    "local",
                    &qualified_name,
                    &serde_json::json!({"query": "runtime evidence"}),
                    &digest,
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        let during = client.lifecycle_snapshot().await;
        let call_result = call.await.expect("Tool task");
        let mut after = client.lifecycle_snapshot().await;
        for _ in 0..100 {
            if after.live_sessions == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            after = client.lifecycle_snapshot().await;
        }
        client.shutdown().await;
        let child_pids = read_pids(&pids);

        assert_eq!(during.live_sessions, 1);
        assert_eq!(during.active_leases, 1);
        assert!(
            call_result.is_ok(),
            "active Tool was evicted: {call_result:?}"
        );
        assert_eq!(after.live_sessions, 0);
        assert_eq!(after.active_leases, 0);
        assert_eq!(after.idle_evictions, 1);
        assert_eq!(after.cached_catalogs, 1);
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "called\n");
        for pid in child_pids {
            assert!(process_exited(pid).await, "session child {pid} survived");
        }
    }

    /// Once the session actor accepted `tools/call`, losing its response channel
    /// cannot prove whether the remote side effect happened. Retrying on a new
    /// stdio process would execute an Unknown-effect Tool twice.
    #[tokio::test]
    async fn accepted_tool_call_with_lost_actor_response_is_never_retried() {
        let state = tempfile::tempdir().expect("fixture state");
        let starts = state.path().join("starts");
        let calls = state.path().join("calls");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let server_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c3);
        let client = StdioMcpClient::new(
            HashMap::from([(
                server_id,
                LocalStdioMcpConfig {
                    command: Path::new("/bin/sh").to_path_buf(),
                    args: vec![script.to_string_lossy().into_owned()],
                    env: BTreeMap::from([
                        (
                            "MCP_START_MARKER".into(),
                            starts.to_string_lossy().into_owned(),
                        ),
                        (
                            "MCP_CALL_MARKER".into(),
                            calls.to_string_lossy().into_owned(),
                        ),
                    ]),
                    cwd: None,
                    protocol_revision: McpProtocolRevision::V2025_06_18,
                    client_capabilities: Default::default(),
                },
            )]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig::default(),
        );
        let listed = serde_json::json!({
            "tools": [{
                "name": "search",
                "description": "Return local runtime evidence",
                "inputSchema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }]
        });
        let catalog = catalog_from_list_result(
            "local",
            &listed,
            std::collections::BTreeSet::from([McpServerCapability::Tools]),
        )
        .expect("fixture catalog");

        let (sender, mut receiver) = mpsc::channel::<SessionRequest>(1);
        let shutdown = CancellationToken::new();
        let actor_calls = calls.clone();
        let task = tokio::spawn(async move {
            let request = receiver.recv().await.expect("accepted Tool request");
            assert!(matches!(
                request.operation,
                SessionOperation::CallTool { .. }
            ));
            std::fs::write(actor_calls, "called\n").expect("record accepted side effect");
            // Simulate a Host/actor failure after the Tool was accepted and its
            // side effect happened, but before a result was durably returned.
            drop(request);
        });
        client.sessions.lock().await.insert(
            server_id,
            SessionHandle {
                sender,
                shutdown,
                task,
                usage: Arc::new(SessionUsage::new()),
            },
        );

        let result = client
            .call_tool(
                server_id,
                "local",
                &catalog.tools[0].qualified_name,
                &serde_json::json!({"query": "runtime evidence"}),
                &catalog.digest,
            )
            .await;
        client.shutdown().await;

        assert!(
            matches!(result, Err(McpFederationError::Unreachable(_))),
            "an accepted ambiguous Tool call was retried: {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&calls).expect("side-effect marker"),
            "called\n",
            "the accepted Tool call ran more than once"
        );
        assert!(
            !starts.exists(),
            "a replacement MCP process was started for an ambiguous Tool call"
        );
    }

    /// A tenant may configure many servers, but an embedded Runtime cannot let
    /// every discovered server become a permanent child process. The least
    /// recently used zero-lease session is reaped while both safe directories
    /// remain cached.
    #[tokio::test]
    async fn session_capacity_evicts_only_an_idle_lru_session() {
        let state = tempfile::tempdir().expect("fixture state");
        let starts = state.path().join("starts");
        let lists = state.path().join("lists");
        let pids = state.path().join("grandchildren");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/stdio_mcp_server.sh")
            .canonicalize()
            .expect("fixture script");
        let first_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c4);
        let second_id = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_00c5);
        let server = || LocalStdioMcpConfig {
            command: Path::new("/bin/sh").to_path_buf(),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([
                (
                    "MCP_START_MARKER".into(),
                    starts.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_LIST_MARKER".into(),
                    lists.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_GRANDCHILD_PID_LOG".into(),
                    pids.to_string_lossy().into_owned(),
                ),
            ]),
            cwd: None,
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: Default::default(),
        };
        let client = StdioMcpClient::new(
            HashMap::from([(first_id, server()), (second_id, server())]),
            Duration::from_secs(2),
            LocalMcpLifecycleConfig {
                catalog_ttl: Duration::from_secs(30),
                session_idle_ttl: Duration::from_secs(30),
                sweep_interval: Duration::from_secs(30),
                max_sessions: 1,
            },
        );

        client
            .list_tools(first_id, "first")
            .await
            .expect("first directory");
        client
            .list_tools(second_id, "second")
            .await
            .expect("second directory");
        let snapshot = client.lifecycle_snapshot().await;
        let child_pids = read_pids(&pids);
        let first_reaped_before_shutdown = process_exited(child_pids[0]).await;
        client.shutdown().await;

        assert!(first_reaped_before_shutdown, "LRU child was not reaped");
        assert_eq!(snapshot.live_sessions, 1);
        assert_eq!(snapshot.active_leases, 0);
        assert_eq!(snapshot.cached_catalogs, 2);
        assert_eq!(snapshot.lru_evictions, 1);
        assert_eq!(
            std::fs::read_to_string(&starts).unwrap(),
            "started\nstarted\n"
        );
        assert_eq!(std::fs::read_to_string(&lists).unwrap(), "listed\nlisted\n");
        for pid in child_pids {
            assert!(process_exited(pid).await, "session child {pid} survived");
        }
    }
}
